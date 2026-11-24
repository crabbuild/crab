use super::{actor, issue_types, read_error};
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
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
mod history;
mod issue_creation;
#[cfg(test)]
mod moved_tests;
mod placements;
mod type_filter;
mod work;

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
            "/api/organizations/{id}/projects/{project}/filter-repositories",
            get(filter_repositories),
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
    let mut type_source = None;
    for linked in &view.repositories {
        let Some(repo) = server.repositories.by_id(*linked) else {
            continue;
        };
        let grant = match app::membership(&server, &principal, &repo).await {
            Ok(grant) => grant,
            Err(Error::NotFound) => continue,
            Err(error) => return Err(error),
        };
        let owned = repo.organization == Some(id);
        if owned && type_source.is_none() {
            type_source = Some(Arc::clone(&repo));
        }
        let mut entry = repository(&repo);
        entry["can_create_issue"] = json!(owned && grant.principal.can_write(repo.id));
        repositories.push(entry);
    }
    let mut types = if type_source.is_some() {
        issue_types::active(&server, Some(id)).await?.definitions
    } else {
        Vec::new()
    };
    let current = project(&server, id, actor, selected).await?;
    if let Some(repo) = type_source {
        match app::membership(&server, &principal, &repo).await {
            Ok(_) => (),
            Err(Error::NotFound) => types.clear(),
            Err(error) => return Err(error),
        }
    }
    app::account(&principal)?;
    if current.project.version != view.project.version {
        return Err(Error::PageChanged);
    }
    Ok(Json(
        json!({"project":public_project(&current.project),"can_manage":current.can_manage,"repositories":repositories,"restricted_repositories":view.repositories.len()-repositories.len(),"lead":profiles.first().cloned().flatten(),"issue_types":types}),
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
    issue_type: Option<String>,
    type_cursor: Option<String>,
    group_by: Option<source::grouping::GroupBy>,
}

async fn filter_repositories(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((organization, selected)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let locations = placements::resolve(
        &server,
        &principal,
        organization,
        projects::SourcesQuery {
            actor,
            project: selected,
            item: None,
            include_pulls: false,
        },
    )
    .await?;
    let view = project(&server, organization, actor, selected).await?;
    if view.project.version != locations.project_version {
        return Err(Error::PageChanged);
    }
    let candidates = view
        .repositories
        .into_iter()
        .chain(locations.rows.iter().map(|row| row.repository.id))
        .collect::<std::collections::BTreeSet<_>>();
    locations.verify(&server, &principal).await?;
    let mut options: Vec<_> = stream::iter(candidates)
        .map(|id| accessible(&server, &principal, id))
        .buffer_unordered(8)
        .try_filter_map(futures_util::future::ok)
        .try_collect()
        .await?;
    let current = project(&server, organization, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != locations.project_version {
        return Err(Error::PageChanged);
    }
    options.sort_unstable_by(|a, b| {
        (&a.config.owner, &a.config.name).cmp(&(&b.config.owner, &b.config.name))
    });
    Ok(Json(
        json!({"items":options.iter().map(|repo| repository(repo)).collect::<Vec<_>>()}),
    ))
}

async fn items(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, selected)): Path<(Uuid, Uuid)>,
    Query(input): Query<Filter>,
) -> Result<Json<Value>> {
    let group_by_type = input.group_by == Some(source::grouping::GroupBy::IssueType);
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let type_scope = type_filter::resolve(&server, &principal, id, selected, actor, &input).await?;
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
                item: input.item,
                group_by: input.group_by,
                matched: type_scope.as_ref().map(|filter| filter.matched.clone()),
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    let show_issue_type = group_by_type
        || page
            .view
            .project
            .definition
            .views
            .iter()
            .any(|view| view.show_issue_type);
    let mut grouped: BTreeMap<Uuid, Vec<Subject>> = BTreeMap::new();
    for entry in &page.items {
        let item = &entry.record;
        if let (Some(repo), Some(subject)) = (item.repository, item.item.subject) {
            grouped.entry(repo).or_default().push(subject);
        }
    }
    let mut sources = work::Scope {
        server: &server,
        principal: &principal,
        organization: id,
        show_issue_type,
        type_scope: type_scope.as_ref(),
    }
    .load(grouped)
    .await?;
    if let Some(filter) = &type_scope {
        filter.verify(&server, &principal, id).await?;
    }
    sources.recheck_moves(&server).await?;
    let current = project(&server, id, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != page.view.project.version {
        return Err(Error::PageChanged);
    }
    let rows = page.items.into_iter().map(|entry| {
        let record = entry.record;
        let mut item = record.item;
        let summary = record.repository.and_then(|repo| item.subject.and_then(|subject| sources.find(repo, subject)));
        let restricted = item.subject.is_some() && summary.is_none();
        if restricted {
            item.subject = None;
            item.title = "Private or unavailable item".into();
            item.body.clear();
        }
        let details = summary.map(|row| &row.details);
        let issue_type = summary.and_then(|row| row.issue_type.as_ref());
        json!({
            "item": item,
            "version": entry.version,
            "repository": summary.map(|row| repository(&row.repository)),
            "source_repository": summary.and(record.repository),
            "restricted": restricted,
            "work": details.map(|details| &details.work),
            "blocked": details.is_some_and(|details| details.blocked),
            "issue_type": issue_type,
            "issue_type_version": summary.and_then(|row| row.type_version),
            "issue_type_group": group_by_type.then(|| crate::cells::repository::issues::issue_type::group(
                details.is_some_and(|details| details.issue_type.is_some()),
                issue_type.map(|definition| definition.id),
            )),
            "assignees": summary.map(|row| row.assignees.as_slice()).unwrap_or_default(),
            "parent": details.and_then(|details| details.parent.as_ref()),
            "progress": details.and_then(|details| details.progress.as_ref()),
            "links": details.map(|details| details.links.as_slice()).unwrap_or_default(),
            "more_links": details.is_some_and(|details| details.more_links),
        })
    }).collect::<Vec<_>>();
    let mut issue_type_counts = type_scope
        .as_ref()
        .and_then(|filter| filter.issue_type_counts.clone());
    if let Some(counts) = &mut issue_type_counts {
        let other = page
            .total
            .checked_sub(counts.values().sum())
            .ok_or(Error::CellContract(
                "Issue type groups exceed the project item count",
            ))?;
        if other != 0 {
            counts.insert("other".into(), other);
        }
    }
    Ok(Json(
        json!({"page":{"project":public_project(&current.project),"items":rows,"next":page.next,"counts":page.counts,"total":page.total,"revision":page.view.project.version,"issue_types":sources.types.unwrap_or_default(),"type_cursor":type_scope.map(|filter| filter.cursor),"group_counts":issue_type_counts.or(page.group_counts)},"can_manage":current.can_manage}),
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
    if matches!(input.command, Command::CreateIssue { .. }) {
        return issue_creation::submit(server, principal, id, actor, input, cancel)
            .await
            .map(Json);
    }
    let first = submit_change(&server, &principal, id, actor, &input, cancel).await;
    // A competing planning write can invalidate the address check before it
    // reaches the Organization Cell. The same request remains safe to retry.
    let output = match first {
        Err(Error::Cell(crate::cells::Error::StaleSearch)) => {
            submit_change(&server, &principal, id, actor, &input, cancel).await?
        }
        result => result?,
    };
    if output == projects::Outcome::RequestConflict {
        return Err(Error::Conflict);
    }
    Ok(Json(output))
}

async fn submit_change(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    actor: Uuid,
    input: &Submission,
    cancel: bool,
) -> Result<projects::Outcome> {
    use crate::cells::repository::{
        collaboration::SubjectKind, issues::transfer::publication::Destination,
    };
    let (source_allowed, attachment) = if cancel {
        (false, None)
    } else {
        match &input.command {
            Command::Repository { repository, .. } => (
                accessible(server, principal, *repository).await?.is_some(),
                None,
            ),
            Command::Item {
                project,
                repository: Some(repository),
                item,
                version,
            } => {
                match item.subject {
                    Some(subject) if *version == 0 && subject.kind == SubjectKind::Issue => {
                        app::number(subject.number)?;
                        let attachment =
                            if accessible(server, principal, *repository).await?.is_some() {
                                match placements::check_attachment(
                                    server,
                                    id,
                                    projects::ProjectKey {
                                        actor,
                                        project: *project,
                                    },
                                    Destination {
                                        repository: *repository,
                                        number: subject.number,
                                    },
                                )
                                .await
                                {
                                    Ok(attachment) => attachment,
                                    Err(Error::NotFound) => None,
                                    Err(error) => return Err(error),
                                }
                            } else {
                                None
                            };
                        // Sample the Project before the final live-source read:
                        // concurrent old and new addresses cannot both pass the
                        // Organization version fence for the same placement.
                        let allowed = attachment.is_some()
                            && work::can_use_source(server, principal, *repository, subject, false)
                                .await?;
                        (allowed, attachment.filter(|_| allowed))
                    }
                    Some(subject) => (
                        work::can_use_source(
                            server,
                            principal,
                            *repository,
                            subject,
                            *version != 0,
                        )
                        .await?,
                        None,
                    ),
                    None => (false, None),
                }
            }
            _ => (true, None),
        }
    };
    app::account(principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::command_output(
        cells
            .command::<projects::Change>(
                Address::Organization(id),
                app::mutation_identity()?,
                Request {
                    actor,
                    request_id: input.request_id,
                    command: input.command.clone(),
                    cancel,
                    source_allowed,
                    attachment,
                    source_epoch: None,
                },
            )
            .await,
    )
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
