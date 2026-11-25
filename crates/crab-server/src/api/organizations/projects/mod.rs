use super::{actor, issue_types, read_error, target};
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
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use uuid::Uuid;
#[cfg(test)]
mod access_tests;
mod authority;
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
        .route(
            "/api/organizations/{id}/projects/context",
            get(authority::workspace),
        )
        .route(
            "/api/organizations/{id}/project-policy",
            get(default_access),
        )
        .route("/api/organizations/{id}/projects/commands", post(change))
        .route("/api/organizations/{id}/projects/cancel", post(cancel))
        .route("/api/organizations/{id}/projects/{project}", get(detail))
        .route(
            "/api/organizations/{id}/projects/{project}/access",
            get(access),
        )
        .route(
            "/api/organizations/{id}/projects/{project}/collaborators",
            get(authority::candidates),
        )
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
async fn default_access(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<projects::DefaultAccessPreview>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let preview = cells
        .query::<projects::GetDefaultAccess>(
            target(id)?,
            None,
            crate::cells::organization::Caller { actor },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(preview))
}
async fn access(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, project)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let read = || async {
        let actor = authority::viewer(&server, &principal, id, actor).await?;
        cells
            .query::<projects::GetAccess>(
                Address::Organization(id),
                None,
                projects::ProjectKey { actor, project },
            )
            .await
            .map_err(read_error)?
            .output
            .ok_or(Error::NotFound)
    };
    let initial = read().await?;
    let ids: Vec<_> = initial
        .grants
        .iter()
        .filter_map(|grant| match grant.target {
            projects::AccessTarget::Member { id } | projects::AccessTarget::Outside { id } => {
                Some(id)
            }
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        app::account(&principal)?;
        return Ok(Json(serde_json::to_value(initial)?));
    }
    let profiles = app::query_output(
        cells
            .query::<crate::cells::directory::LookupProfiles>(Address::Directory, None, ids.clone())
            .await,
    )?;
    if profiles.len() != ids.len() {
        return Err(Error::CellContract(
            "Project access profile count is invalid",
        ));
    }
    let names: BTreeMap<_, _> = ids
        .into_iter()
        .zip(profiles)
        .filter_map(|(id, profile)| profile.map(|profile| (id, profile.name)))
        .collect();
    let eligible = authority::eligible(
        &server,
        id,
        initial
            .grants
            .iter()
            .filter_map(|grant| match grant.target {
                projects::AccessTarget::Outside { id } if grant.active => Some(id),
                _ => None,
            })
            .collect(),
    )
    .await?;
    let mut current = read().await?;
    if current.version != initial.version {
        return Err(Error::PageChanged);
    }
    for grant in &mut current.grants {
        match grant.target {
            projects::AccessTarget::Member { id } => grant.label = names.get(&id).cloned(),
            projects::AccessTarget::Outside { id } => {
                grant.label = names.get(&id).cloned();
                grant.active &= eligible.contains(&id);
            }
            _ => (),
        }
    }
    app::account(&principal)?;
    Ok(Json(serde_json::to_value(current)?))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Collection {
    archived: bool,
    search: String,
    after: Option<Uuid>,
    revision: Option<u64>,
    team: Option<Uuid>,
    repository: Option<Uuid>,
    linkable: bool,
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
    let actor = authority::caller(&server, &principal, id).await?;
    if let Some(repository) = input.repository {
        owned_repository(&server, &principal, id, repository).await?;
    }
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
                team: input.team,
                repository: input.repository,
                linkable: input.linkable,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    if let Some(repository) = input.repository {
        owned_repository(&server, &principal, id, repository).await?;
    }
    app::account(&principal)?;
    if actor.guest()
        && !authority::viewer(&server, &principal, id, actor.id())
            .await?
            .guest()
    {
        return Err(Error::PageChanged);
    }
    Ok(Json(
        json!({"items":page.items.iter().map(public_project).collect::<Vec<_>>(),"next":page.next,"revision":page.revision,"can_create":page.can_create}),
    ))
}
async fn project(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    actor: projects::AccessActor,
    id: Uuid,
) -> Result<projects::View> {
    let actor = authority::viewer(server, principal, organization, actor.id()).await?;
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
async fn owned_repository(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    repository: Uuid,
) -> Result<Arc<Repository>> {
    let repo = server
        .repositories
        .by_id(repository)
        .filter(|repo| repo.organization == Some(organization))
        .ok_or(Error::NotFound)?;
    let current = if principal.is_local() {
        // Local operators bypass repository grant lookup, but a discovery
        // link still needs the current Catalog identity and ownership epoch.
        let snapshot = server
            .catalog()
            .ok_or(Error::NotFound)?
            .membership(&repo.config.owner, &repo.config.name)
            .await
            .map_err(app::repository_access_error)?;
        if snapshot.name != repo.config.name {
            return Err(Error::NotFound);
        }
        app::membership_snapshot(server, principal, &repo, snapshot).await?
    } else {
        app::membership(server, principal, &repo).await?
    };
    current
        .principal
        .can_read(repository)
        .then_some(repo)
        .ok_or(Error::NotFound)
}
fn repository(repo: &Repository) -> Value {
    json!({"id":repo.id,"owner":repo.config.owner,"name":repo.config.name})
}
async fn detail(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, selected)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?.into();
    let view = project(&server, &principal, id, actor, selected).await?;
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
    let current = project(&server, &principal, id, actor, selected).await?;
    if let Some(repo) = type_source {
        match app::membership(&server, &principal, &repo).await {
            Ok(_) => (),
            Err(Error::NotFound) => types.clear(),
            Err(error) => return Err(error),
        }
    }
    if current.project.version != view.project.version {
        return Err(Error::PageChanged);
    }
    let configured = current.project.default_repository;
    let mut default_repository = None;
    if let Some(default) = configured
        && let Some(index) = repositories
            .iter()
            .position(|entry| entry["id"] == json!(default.id))
    {
        match owned_repository(&server, &principal, id, default.id).await {
            Ok(repo) if repo.ownership_epoch == default.ownership_epoch => {
                if crate::repository::settings::load_lifecycle(&server, &repo)
                    .await?
                    .archived
                {
                    repositories[index]["can_create_issue"] = json!(false);
                } else {
                    default_repository = Some(default.id);
                }
            }
            Ok(_) => (),
            Err(Error::NotFound) => {
                repositories.remove(index);
            }
            Err(error) => return Err(error),
        }
    }
    app::account(&principal)?;
    Ok(Json(
        json!({"project":public_project(&current.project),"can_manage":current.can_manage,"can_admin":current.can_admin,"repositories":repositories,"restricted_repositories":view.repositories.len()-repositories.len(),"lead":profiles.first().cloned().flatten(),"issue_types":types,"default_repository":default_repository,"default_repository_configured":configured.is_some()}),
    ))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    after: Option<Uuid>,
    version: Option<u64>,
    status: Option<String>,
    priority: Option<u8>,
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
    let actor = authority::caller(&server, &principal, organization).await?;
    let locations = placements::resolve(
        &server,
        &principal,
        organization,
        projects::SourcesQuery {
            actor,
            project: selected,
            item: None,
            include_pulls: false,
            priority: None,
        },
    )
    .await?;
    let view = project(&server, &principal, organization, actor, selected).await?;
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
    let current = project(&server, &principal, organization, actor, selected).await?;
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
    let actor = authority::caller(&server, &principal, id).await?;
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
                priority: input.priority,
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
    let current = project(&server, &principal, id, actor, selected).await?;
    app::account(&principal)?;
    if current.project.version != page.view.project.version {
        return Err(Error::PageChanged);
    }
    let free_form_group = match input.group_by {
        Some(source::grouping::GroupBy::Field(id)) => current
            .project
            .definition
            .fields
            .iter()
            .find(|field| field.id == id)
            .filter(|field| !matches!(field.definition, source::FieldKind::Select { .. }))
            .map(|field| field.id),
        _ => None,
    };
    let mut visible_groups = BTreeSet::new();
    let rows = page.items.into_iter().map(|entry| {
        let record = entry.record;
        let mut item = record.item;
        let summary = record.repository.and_then(|repo| item.subject.and_then(|subject| sources.find(repo, subject)));
        let restricted = item.subject.is_some() && summary.is_none();
        if restricted {
            item.subject = None;
            item.title = "Private or unavailable item".into();
            item.body.clear();
            item.values.clear();
        } else if let Some(field) = free_form_group
            && let Some(value) = item.values.get(&field)
        {
            visible_groups.insert(value.clone());
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
    let mut group_counts = page.group_counts;
    if free_form_group.is_some()
        && let Some(counts) = &mut group_counts
    {
        // Free-form group names can themselves contain private item content.
        counts.retain(|value, _| value.is_empty() || visible_groups.contains(value));
    }
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
        json!({"page":{"project":public_project(&current.project),"items":rows,"next":page.next,"counts":page.counts,"total":page.total,"revision":page.view.project.version,"issue_types":sources.types.unwrap_or_default(),"type_cursor":type_scope.map(|filter| filter.cursor),"group_counts":issue_type_counts.or(group_counts)},"can_manage":current.can_manage}),
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
    let actor = actor(&server, &principal).await?.into();
    let view = project(&server, &principal, id, actor, selected).await?;
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
    let current = project(&server, &principal, id, actor, selected).await?;
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
    let actor = authority::viewer(&server, &principal, id, actor).await?;
    if !cancel
        && (actor.guest()
            || matches!(
                input.command,
                Command::BulkFields { .. }
                    | Command::SetAccess {
                        target: projects::AccessTarget::Outside { .. },
                        ..
                    }
                    | Command::LinkRepository { .. }
                    | Command::SetDefaultRepository { .. }
            ))
    {
        // Repository access and ownership can change guest qualification and
        // source selection. Check both under the publication policy lease.
        return super::properties::policy_write(server, id, move |server, _, cancel| async move {
            let actor = authority::viewer(&server, &principal, id, actor.id()).await?;
            let output =
                submit_change(&server, &principal, id, actor, &input, false, Some(&cancel)).await?;
            checked_outcome(output)
        })
        .await;
    }
    let first = submit_change(&server, &principal, id, actor, &input, cancel, None).await;
    // A competing planning write can invalidate the address check before it
    // reaches the Organization Cell. The same request remains safe to retry.
    let output = match first {
        Err(Error::Cell(crate::cells::Error::StaleSearch)) => {
            submit_change(&server, &principal, id, actor, &input, cancel, None).await?
        }
        result => result?,
    };
    checked_outcome(output)
}

fn checked_outcome(output: projects::Outcome) -> Result<Json<projects::Outcome>> {
    if output == projects::Outcome::RequestConflict {
        return Err(Error::Conflict);
    }
    Ok(Json(output))
}

async fn bulk_sources_allowed(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    actor: projects::AccessActor,
    project: Uuid,
    items: &[crate::cells::planning::bulk::Edit],
) -> Result<bool> {
    if !crate::cells::planning::bulk::valid(items) {
        return Ok(false);
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::query_output(
        cells
            .query::<projects::BulkSources>(
                Address::Organization(organization),
                None,
                projects::BulkSourceQuery {
                    actor,
                    project,
                    items: items.iter().map(|edit| edit.item).collect(),
                },
            )
            .await,
    );
    let sources = match result {
        Ok(Some(sources)) => sources,
        Ok(None) | Err(Error::Cell(crate::cells::Error::AccessDenied)) => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut checks = stream::iter(
        sources
            .into_iter()
            .map(|(repo, subject)| work::can_use_source(server, principal, repo, subject, true)),
    )
    .buffer_unordered(8);
    while let Some(allowed) = checks.try_next().await? {
        if !allowed {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn submit_change(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    actor: projects::AccessActor,
    input: &Submission,
    cancel: bool,
    policy: Option<&tokio_util::sync::CancellationToken>,
) -> Result<projects::Outcome> {
    use crate::cells::repository::{
        collaboration::SubjectKind, issues::transfer::publication::Destination,
    };
    let mut source_epoch = None;
    let (source_allowed, attachment) = if cancel {
        (false, None)
    } else {
        match &input.command {
            Command::Repository { repository, .. } => (
                accessible(server, principal, *repository).await?.is_some(),
                None,
            ),
            Command::LinkRepository { repository, .. } => {
                let allowed = match owned_repository(server, principal, id, *repository).await {
                    Ok(_) => true,
                    Err(Error::NotFound) => false,
                    Err(error) => return Err(error),
                };
                (allowed, None)
            }
            Command::SetDefaultRepository {
                repository: Some(repository),
                ..
            } => {
                match owned_repository(server, principal, id, *repository).await {
                    Ok(repo) => {
                        if !crate::repository::settings::load_lifecycle(server, &repo)
                            .await?
                            .archived
                        {
                            source_epoch = Some(repo.ownership_epoch);
                        }
                    }
                    Err(Error::NotFound) => (),
                    Err(error) => return Err(error),
                }
                (source_epoch.is_some(), None)
            }
            Command::BulkFields { project, items } => (
                bulk_sources_allowed(server, principal, id, actor, *project, items).await?,
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
    let eligible_outside = match input.command {
        Command::SetAccess {
            target: projects::AccessTarget::Outside { id: outsider },
            role: Some(role),
            ..
        } if !cancel && role != projects::AccessRole::None => {
            authority::eligible(server, id, vec![outsider])
                .await?
                .into_iter()
                .next()
        }
        _ => None,
    };
    app::account(principal)?;
    if policy.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return Err(Error::MergeBusy);
    }
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
                    source_epoch,
                    eligible_outside,
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
