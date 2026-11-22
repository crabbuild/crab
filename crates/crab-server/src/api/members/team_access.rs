use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    catalog::{CatalogStore, MembershipActor, MembershipSnapshot, OrganizationAccess, TeamGrant},
    server::Server,
};

#[derive(Serialize)]
struct StateView {
    revision: u64,
    organization: OrganizationAccess,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    expected_revision: u64,
    teams: Vec<TeamGrant>,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/team-access",
        get(list).put(replace),
    )
}

async fn lookup(
    server: &Server,
    principal: &Principal,
    route: &(String, String),
) -> Result<(CatalogStore, MembershipSnapshot, OrganizationAccess, Uuid)> {
    let qualified = app::account(principal)?;
    let repository = app::repository(server, principal, route)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let snapshot = catalog
        .membership(&route.0, &route.1)
        .await
        .map_err(app::repository_access_error)?;
    if snapshot.repository_id != repository.id {
        return Err(Error::NotFound);
    }
    let organization = snapshot.organization.clone().ok_or(Error::NotFound)?;
    let actor = app::registered_actor(server, qualified)
        .await?
        .ok_or(Error::NotFound)?;
    Ok((catalog, snapshot, organization, actor))
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(route): Path<(String, String)>,
) -> Result<Json<StateView>> {
    let (_, snapshot, organization, actor) = lookup(&server, &principal, &route).await?;
    if !app::repository_team_access(&server, &organization, Some(actor))
        .await?
        .owner
    {
        return Err(Error::NotFound);
    }
    app::account(&principal)?;
    Ok(Json(StateView {
        revision: snapshot.revision,
        organization,
    }))
}

async fn replace(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(route): Path<(String, String)>,
    Json(input): Json<Update>,
) -> Result<Json<StateView>> {
    let (catalog, snapshot, mut organization, actor) = lookup(&server, &principal, &route).await?;
    let before = organization.clone();
    organization.teams = input.teams;
    organization
        .validate()
        .map_err(app::repository_access_error)?;
    let current = app::repository_team_access(&server, &organization, Some(actor)).await?;
    if !current.owner {
        return Err(Error::NotFound);
    }
    if input.expected_revision != snapshot.revision {
        return Err(Error::Conflict);
    }
    if current.teams.len() != organization.teams.len() {
        return Err(Error::Invalid(
            "Every grant must name a current team in the owning organization",
        ));
    }
    let identity = app::account(&principal)?;
    let record = catalog
        .set_organization_access(
            &route.0,
            &route.1,
            snapshot.repository_id,
            input.expected_revision,
            organization,
            MembershipActor {
                issuer: identity.issuer,
                subject: identity.subject,
            },
        )
        .await
        .map_err(app::repository_access_error)?;
    let organization = record.organization.ok_or(Error::NotFound)?;
    let revision = snapshot.revision + u64::from(organization != before);
    Ok(Json(StateView {
        revision,
        organization,
    }))
}
