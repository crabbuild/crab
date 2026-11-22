use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    RepositoryAccess, app,
    auth::Principal,
    catalog::{CatalogError, CatalogRecord, MembershipActor},
    repository::access::visible_team_names,
    server::Server,
};

use super::{authorized, boxed_error, error, membership_error};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/settings/team-access", get(list))
        .route(
            "/api/repos/{owner}/{name}/settings/team-access/{team}",
            put(grant).delete(remove),
        )
}

#[derive(Serialize)]
struct StateView {
    revision: u64,
    grants: Vec<GrantView>,
}

#[derive(Serialize)]
struct GrantView {
    team: Uuid,
    name: Option<String>,
    access: RepositoryAccess,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantInput {
    expected_revision: u64,
    access: RepositoryAccess,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveInput {
    expected_revision: u64,
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let result = async {
        let (_, document) = authorized(&server, &principal, &owner, &name).await?;
        let (record, organization) = destination(&document.repositories, &owner, &name)?;
        let snapshot = document
            .membership_snapshot(record)
            .map_err(catalog_error)?;
        if snapshot.teams.is_empty() {
            return Ok(Json(StateView {
                revision: document.version,
                grants: Vec::new(),
            })
            .into_response());
        }
        let visible = visible_team_names(
            &server,
            &principal,
            organization,
            snapshot.teams.iter().map(|grant| grant.team).collect(),
        )
        .await
        .map_err(membership_error)?;
        recheck(&server, &principal, &owner, &name, document.version).await?;
        // Names and even the existence of secret teams are visible only to
        // current organization owners or members of that team.
        let grants = snapshot
            .teams
            .iter()
            .filter_map(|grant| {
                let name = visible
                    .names
                    .iter()
                    .find(|(id, _)| *id == grant.team)
                    .map(|(_, name)| name.clone());
                if name.is_none() && !visible.owner {
                    return None;
                }
                Some(GrantView {
                    team: grant.team,
                    name,
                    access: grant.access,
                })
            })
            .collect();
        Ok(Json(StateView {
            revision: document.version,
            grants,
        })
        .into_response())
    }
    .await;
    result.unwrap_or_else(|response: Box<Response>| *response)
}

async fn grant(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String, Uuid)>,
    Json(input): Json<GrantInput>,
) -> Response {
    change(
        &server,
        &principal,
        key,
        input.expected_revision,
        Some(input.access),
    )
    .await
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String, Uuid)>,
    Json(input): Json<RemoveInput>,
) -> Response {
    change(&server, &principal, key, input.expected_revision, None).await
}

async fn change(
    server: &Server,
    principal: &Principal,
    (owner, name, team): (String, String, Uuid),
    revision: u64,
    access: Option<RepositoryAccess>,
) -> Response {
    let result = async {
        let (catalog, document) = authorized(server, principal, &owner, &name).await?;
        let (record, organization) = destination(&document.repositories, &owner, &name)?;
        if record.forked_from.is_some() {
            return Err(boxed_error(
                StatusCode::FORBIDDEN,
                "fork_access_inherited",
                "Manage fork access on the source repository.",
            ));
        }
        if team.is_nil() {
            return Err(unknown_team());
        }
        if revision != document.version {
            return Err(catalog_error(CatalogError::Conflict));
        }
        let visible = visible_team_names(server, principal, organization, vec![team])
            .await
            .map_err(membership_error)?;
        let assigned = record.teams.iter().any(|grant| grant.team == team);
        // Owners may clean up a deleted team's grant. UUID tombstones never
        // qualify a replacement team for the old access.
        if visible.names.is_empty() && !(access.is_none() && assigned && visible.owner) {
            return Err(unknown_team());
        }
        recheck(server, principal, &owner, &name, revision).await?;
        let actor = app::account(principal).map_err(membership_error)?;
        let revision = catalog
            .set_team_access(
                record.id,
                revision,
                team,
                access,
                MembershipActor {
                    issuer: actor.issuer,
                    subject: actor.subject,
                },
            )
            .await
            .map_err(catalog_error)?;
        Ok(Json(json!({"revision":revision})).into_response())
    }
    .await;
    result.unwrap_or_else(|response| *response)
}

async fn recheck(
    server: &Server,
    principal: &Principal,
    owner: &str,
    name: &str,
    revision: u64,
) -> Result<(), Box<Response>> {
    let (_, current) = authorized(server, principal, owner, name).await?;
    if current.version != revision {
        return Err(catalog_error(CatalogError::Conflict));
    }
    Ok(())
}

fn destination<'a>(
    records: &'a [CatalogRecord],
    owner: &str,
    name: &str,
) -> Result<(&'a CatalogRecord, Uuid), Box<Response>> {
    records
        .iter()
        .find(|record| {
            record.owner.eq_ignore_ascii_case(owner) && record.name.eq_ignore_ascii_case(name)
        })
        .and_then(|record| {
            record
                .organization
                .map(|organization| (record, organization))
        })
        .ok_or_else(|| {
            boxed_error(
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Organization repository not found.",
            )
        })
}

fn unknown_team() -> Box<Response> {
    boxed_error(
        StatusCode::NOT_FOUND,
        "team_not_found",
        "Organization team not found.",
    )
}

fn catalog_error(failure: CatalogError) -> Box<Response> {
    Box::new(match failure {
        CatalogError::Conflict => error(
            StatusCode::CONFLICT,
            "team_access_conflict",
            "Repository access changed. Reload before editing team access.",
        ),
        CatalogError::Invalid(message) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_team_access",
            message,
        ),
        CatalogError::NotFound => error(
            StatusCode::NOT_FOUND,
            "repository_not_found",
            "Organization repository not found.",
        ),
        other => *membership_error(app::Error::Membership(Box::new(other))),
    })
}
