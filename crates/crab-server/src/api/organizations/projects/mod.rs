use super::{actor, read_error};
use crate::{
    api::write::ResponseError,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        organization::projects::{self, Command, Request},
        repository::{collaboration::Subject, projects as source},
    },
    repository::Repository,
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
mod history;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/projects", get(list))
        .route("/api/organizations/{id}/projects/commands", post(change))
        .route("/api/organizations/{id}/projects/cancel", post(cancel))
        .route("/api/organizations/{id}/projects/{project}", get(detail))
        .route(
            "/api/organizations/{id}/projects/{project}/history",
            get(history::read),
        )
        .route(
            "/api/organizations/{id}/projects/{project}/items",
            get(items),
        )
        .route(
            "/api/organizations/{id}/projects/{project}/candidates",
            get(candidates),
        )
        .layer(axum::extract::DefaultBodyLimit::max(96 * 1024))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Collection {
    archived: bool,
    search: String,
    after: Option<Uuid>,
    revision: Option<u64>,
}
fn public_project(project: &projects::Project) -> Value {
    // Creator membership incarnations protect writes inside the native Cell;
    // browser metadata must not become a second representation of grant state.
    json!({"id":project.id,"definition":project.definition,"properties":project.properties,"version":project.version,"archived":project.archived,"created_at":project.created_at,"updated_at":project.updated_at})
}
async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(input): Query<Collection>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let page = cells
        .query::<projects::List>(
            Address::Organization(id),
            None,
            projects::ListQuery {
                actor,
                archived: input.archived,
                search: input.search,
                after: input.after,
                revision: input.revision,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(
        json!({"items":page.items.iter().map(public_project).collect::<Vec<_>>(),"next":page.next,"revision":page.revision,"can_create":true}),
    ))
}
async fn project(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
    id: Uuid,
) -> Result<projects::View> {
    server
        .cells()
        .ok_or(Error::CellUnavailable)?
        .query::<projects::Get>(
            Address::Organization(organization),
            None,
            projects::ProjectKey { actor, project: id },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)
}
async fn accessible(
    server: &Server,
    principal: &Principal,
    id: Uuid,
) -> Result<Option<Arc<Repository>>> {
    let Some(repo) = server.repositories.by_id(id) else {
        return Ok(None);
    };
    // Organization routes have no repository grant from the HTTP boundary.
    // Resolve each source's current membership before deciding visibility.
    match app::membership(server, principal, &repo).await {
        Ok(_) => Ok(Some(repo)),
        Err(Error::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}
fn repository(repo: &Repository) -> Value {
    json!({"id":repo.id,"owner":repo.config.owner,"name":repo.config.name})
}
async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, selected)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let view = project(&server, id, actor, selected).await?;
    let profiles = app::query_output(
        server
            .cells()
            .ok_or(Error::CellUnavailable)?
            .query::<crate::cells::directory::LookupProfiles>(
                Address::Directory,
                None,
                view.project.properties.lead.into_iter().collect(),
            )
            .await,
    )?;
    let mut repositories = Vec::new();
    for linked in &view.repositories {
        if let Some(repo) = accessible(&server, &principal, *linked).await? {
            repositories.push(repository(&repo));
        }
    }
    let current = project(&server, id, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != view.project.version {
        return Err(Error::PageChanged);
    }
    Ok(Json(
        json!({"project":public_project(&current.project),"can_manage":current.can_manage,"repositories":repositories,"restricted_repositories":view.repositories.len()-repositories.len(),"lead":profiles.first().cloned().flatten()}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    after: Option<Uuid>,
    version: Option<u64>,
    status: Option<String>,
    repository: Option<Uuid>,
    item: Option<Uuid>,
}
async fn items(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, selected)): Path<(Uuid, Uuid)>,
    Query(input): Query<Filter>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let page = cells
        .query::<projects::Items>(
            Address::Organization(id),
            None,
            projects::ItemsQuery {
                actor,
                project: selected,
                after: input.after,
                version: input.version,
                status: input.status,
                repository: input.repository,
                item: input.item,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    let mut grouped: BTreeMap<Uuid, Vec<Subject>> = BTreeMap::new();
    for entry in &page.items {
        let item = &entry.record;
        if let (Some(repo), Some(subject)) = (item.repository, item.item.subject) {
            grouped.entry(repo).or_default().push(subject);
        }
    }
    let mut work = BTreeMap::new();
    let mut repositories = BTreeMap::new();
    for (id, subjects) in grouped {
        let Some(repo) = accessible(&server, &principal, id).await? else {
            continue;
        };
        let cell = app::repository_cell(&server, &repo)?;
        let found = app::query_output(
            cell.client
                .query::<source::WorkSummaries>(&cell.target, None, subjects.clone())
                .await,
        )?;
        if found.len() != subjects.len() {
            return Err(Error::CellContract(
                "Project work lookup returned an invalid result count",
            ));
        }
        let selection = match crate::api::assignees::Selection::load(
            &server,
            &principal,
            &repo,
            found
                .iter()
                .flatten()
                .flat_map(|details| details.assignees.iter()),
        )
        .await
        {
            Ok(selection) => selection,
            Err(Error::NotFound) => continue,
            Err(error) => return Err(error),
        };
        // Repository grants remain independent of organization membership. Refresh
        // after source reads, then recheck project authority before returning rows.
        if accessible(&server, &principal, id).await?.is_none() {
            continue;
        }
        for (subject, summary) in subjects.into_iter().zip(found) {
            if let Some(summary) = summary {
                let assignees = selection.view(&summary.assignees);
                work.insert(
                    (id, subject.kind as u8, subject.number),
                    (summary, assignees),
                );
            }
        }
        repositories.insert(id, repository(&repo));
    }
    let rows=page.items.into_iter().map(|entry|{
        let record = entry.record;
        let mut item=record.item;
        let summary=record.repository.and_then(|repo|item.subject.and_then(|subject|work.get(&(repo,subject.kind as u8,subject.number))));
        let restricted=item.subject.is_some() && summary.is_none();
        if restricted { item.subject=None;item.title="Private or unavailable item".into();item.body.clear(); }
        json!({"item":item,"version":entry.version,"repository":if restricted{None}else{record.repository.and_then(|id|repositories.get(&id))},"restricted":restricted,"work":summary.map(|(details,_)|&details.work),"assignees":summary.map(|(_,assignees)|assignees).cloned().unwrap_or_default(),"parent":summary.and_then(|(details,_)|details.parent.as_ref()),"progress":summary.and_then(|(details,_)|details.progress.as_ref()),"links":summary.map(|(details,_)|details.links.as_slice()).unwrap_or_default(),"more_links":summary.is_some_and(|(details,_)|details.more_links)})
    }).collect::<Vec<_>>();
    let current = project(&server, id, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != page.view.project.version {
        return Err(Error::PageChanged);
    }
    Ok(Json(
        json!({"page":{"project":public_project(&current.project),"items":rows,"next":page.next,"counts":page.counts,"total":page.total,"revision":page.view.project.version},"can_manage":current.can_manage}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateFilter {
    repository: Uuid,
    #[serde(default)]
    q: String,
}
async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, selected)): Path<(Uuid, Uuid)>,
    Query(input): Query<CandidateFilter>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let view = project(&server, id, actor, selected).await?;
    if !view.can_manage || !view.repositories.contains(&input.repository) {
        return Err(Error::NotFound);
    }
    let repo = accessible(&server, &principal, input.repository)
        .await?
        .ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repo)?;
    let items = app::query_output(
        cell.client
            .query::<source::Candidates>(
                &cell.target,
                None,
                source::CandidateQuery {
                    q: input.q,
                    kind: None,
                },
            )
            .await,
    )?;
    accessible(&server, &principal, input.repository)
        .await?
        .ok_or(Error::NotFound)?;
    let current = project(&server, id, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != view.project.version || !current.can_manage {
        return Err(Error::PageChanged);
    }
    Ok(Json(json!({"items":items})))
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
    id: Uuid,
    input: Submission,
    cancel: bool,
) -> Result<Json<projects::Outcome>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let source_allowed = if cancel {
        false
    } else {
        match &input.command {
            Command::Repository { repository, .. } => accessible(&server, &principal, *repository)
                .await?
                .is_some(),
            Command::Item {
                repository: Some(repository),
                item,
                ..
            } => {
                if let Some(repo) = accessible(&server, &principal, *repository).await? {
                    let cell = app::repository_cell(&server, &repo)?;
                    let subjects = item.subject.into_iter().collect::<Vec<_>>();
                    let summaries = app::query_output(
                        cell.client
                            .query::<source::WorkSummaries>(&cell.target, None, subjects)
                            .await,
                    )?;
                    summaries.len() == 1
                        && summaries[0].is_some()
                        && accessible(&server, &principal, *repository)
                            .await?
                            .is_some()
                } else {
                    false
                }
            }
            _ => true,
        }
    };
    app::account(&principal)?;
    let output = app::command_output(
        cells
            .command::<projects::Change>(
                Address::Organization(id),
                app::mutation_identity()?,
                Request {
                    actor,
                    request_id: input.request_id,
                    command: input.command,
                    cancel,
                    source_allowed,
                },
            )
            .await,
    )?;
    if output == projects::Outcome::RequestConflict {
        return Err(Error::Conflict);
    }
    Ok(Json(output))
}
async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<Submission>, JsonRejection>,
) -> std::result::Result<Json<projects::Outcome>, ResponseError> {
    let Json(input) = input
        .map_err(Error::from)
        .map_err(crate::api::write::rejected)?;
    submit(server, principal, id, input, false)
        .await
        .map_err(|error| error.into_response().into())
}
async fn cancel(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<Submission>, JsonRejection>,
) -> std::result::Result<Json<projects::Outcome>, ResponseError> {
    let Json(input) = input
        .map_err(Error::from)
        .map_err(crate::api::write::rejected)?;
    submit(server, principal, id, input, true)
        .await
        .map_err(|error| error.into_response().into())
}

#[cfg(test)]
mod tests;
