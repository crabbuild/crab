use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query, State, rejection::JsonRejection},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::organizations,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        self, Address,
        organization::{Caller, GetOrganization, Role, teams},
        repository::collaboration::mention_settings::{self, Settings},
    },
    repository::Repository,
    server::Server,
};

async fn settings(server: &Server, repo: &Repository) -> Result<Settings> {
    let routed = app::repository_cell(server, repo)?;
    app::query_output(
        routed
            .client
            .query::<mention_settings::Get>(&routed.target, None, ())
            .await,
    )
}

async fn require_admin(server: &Server, principal: &Principal, repo: &Repository) -> Result<()> {
    let current = app::membership(server, principal, repo).await?;
    if current.principal.can_admin(repo.id) {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}

async fn organization_access(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
) -> Result<Option<(Role, u64)>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<GetOrganization>(Address::Organization(organization), None, Caller { actor })
        .await
    {
        Ok(snapshot) => Ok(snapshot
            .output
            .map(|view| (view.role, view.organization.version))),
        Err(cells::Error::NotInitialized | cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(Error::Cell(error)),
    }
}

async fn closed_team(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
    team: Uuid,
) -> Result<Option<teams::Team>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<teams::Get>(
            Address::Organization(organization),
            None,
            teams::Key { actor, team },
        )
        .await
    {
        Ok(snapshot) => Ok(snapshot.output.and_then(|view| {
            (view.team.visibility == teams::Visibility::Closed).then_some(view.team)
        })),
        Err(cells::Error::NotInitialized | cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(super) async fn get_settings(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Settings>> {
    let repo = app::repository(&server, &principal, &key)?;
    require_admin(&server, &principal, &repo).await?;
    let stored = settings(&server, &repo).await?;
    require_admin(&server, &principal, &repo).await?;
    Ok(Json(stored))
}

pub(super) async fn put_settings(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Settings>, JsonRejection>,
) -> Result<Json<Settings>> {
    let repo = app::repository(&server, &principal, &key)?;
    require_admin(&server, &principal, &repo).await?;
    let Json(input) = input.map_err(Error::Body)?;
    input.validate().map_err(Error::Invalid)?;
    if let Some(organization) = input.organization {
        let actor = organizations::actor(&server, &principal).await?;
        let Some((Role::Owner, revision)) =
            organization_access(&server, organization, actor).await?
        else {
            return Err(Error::NotFound);
        };
        let teams: Vec<_> = input.teams.iter().map(|alias| alias.id).collect();
        stream::iter(teams)
            .map(|team| {
                let server = &server;
                async move {
                    closed_team(server, organization, actor, team)
                        .await?
                        .ok_or(Error::Invalid(
                            "Aliases must point to visible teams in the selected organization",
                        ))
                        .map(|_| ())
                }
            })
            .buffer_unordered(4)
            .try_for_each(|()| async { Ok(()) })
            .await?;
        if organization_access(&server, organization, actor).await? != Some((Role::Owner, revision))
        {
            return Err(Error::Conflict);
        }
    }
    require_admin(&server, &principal, &repo).await?;
    let routed = app::repository_cell(&server, &repo)?;
    match app::command_output(
        routed
            .client
            .command::<mention_settings::Set>(&routed.target, app::mutation_identity()?, input)
            .await,
    )? {
        mention_settings::Outcome::Saved(stored) => {
            require_admin(&server, &principal, &repo).await?;
            Ok(Json(stored))
        }
        mention_settings::Outcome::Conflict => Err(Error::Conflict),
        mention_settings::Outcome::Archived => Err(Error::Archived),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    repository_id: Uuid,
    #[serde(default)]
    search: String,
}

#[derive(Serialize)]
struct TeamSuggestion {
    id: Uuid,
    organization: Uuid,
    handle: String,
    name: String,
}

#[derive(Default, Serialize)]
pub(super) struct Suggestions {
    items: Vec<TeamSuggestion>,
}

pub(super) async fn complete(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Suggestions>> {
    let repo = app::repository(&server, &principal, &key)?;
    if repo.id != params.repository_id {
        return Err(Error::NotFound);
    }
    if params.search.len() > 129 || params.search.chars().any(char::is_control) {
        return Err(Error::Invalid("Team mention search is invalid"));
    }
    let binding = settings(&server, &repo).await?;
    let Some(organization) = binding.organization else {
        return Ok(Json(Suggestions::default()));
    };
    let actor = match organizations::actor(&server, &principal).await {
        Ok(actor) => actor,
        Err(Error::NotFound) => return Ok(Json(Suggestions::default())),
        Err(error) => return Err(error),
    };
    let Some(access) = organization_access(&server, organization, actor).await? else {
        return Ok(Json(Suggestions::default()));
    };
    let search = params.search.trim().to_ascii_lowercase();
    let namespace = &binding.namespace;
    let candidates: Vec<_> = binding
        .teams
        .into_iter()
        .filter_map(|alias| {
            let handle = format!("{namespace}/{}", alias.slug);
            handle.contains(&search).then_some((alias.id, handle))
        })
        .collect();
    let mut matches: Vec<_> =
        stream::iter(candidates)
            .map(|(id, handle)| {
                let server = &server;
                async move {
                    Ok::<_, Error>(closed_team(server, organization, actor, id).await?.map(
                        |team| TeamSuggestion {
                            id,
                            organization,
                            handle,
                            name: team.name,
                        },
                    ))
                }
            })
            .buffer_unordered(4)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten()
            .collect();
    // Keep names from one current organization revision and one repository
    // binding. A secret, deleted, or disconnected team cannot leak a name.
    if organization_access(&server, organization, actor).await? != Some(access)
        || settings(&server, &repo).await?.version != binding.version
    {
        return Err(Error::PageChanged);
    }
    app::membership(&server, &principal, &repo).await?;
    matches.sort_unstable_by(|left, right| {
        (!left.handle.starts_with(&search), &left.handle)
            .cmp(&(!right.handle.starts_with(&search), &right.handle))
    });
    matches.truncate(12);
    Ok(Json(Suggestions { items: matches }))
}
