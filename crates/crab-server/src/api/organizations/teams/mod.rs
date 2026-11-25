use super::{actor, read_error, target};
use crate::api::catalog_browse::{self, Scope, Search};
use crate::{
    api::write,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, LookupProfiles},
        organization::{Caller, teams as native},
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct TeamListing {
    #[serde(flatten)]
    page: super::OrganizationPage<native::Summary>,
    can_create: bool,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct TeamMember {
    actor: Uuid,
    role: Option<native::Role>,
    version: u64,
    profile: Option<ActorSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    inherited: Option<bool>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct TeamRoster {
    #[serde(flatten)]
    page: super::OrganizationPage<TeamMember>,
    can_manage: bool,
}

#[derive(Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamAcknowledgement"))]
pub(in crate::api) enum Acknowledgement {
    Applied { id: Uuid },
    Cancelled { id: Uuid },
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/teams", get(list))
        .route(
            "/api/organizations/{id}/teams/hierarchy-requests",
            get(hierarchy_requests),
        )
        .route("/api/organizations/{id}/teams/policy", get(policy))
        .route(
            "/api/organizations/{id}/teams/commands",
            axum::routing::post(apply),
        )
        .route(
            "/api/organizations/{id}/teams/cancel",
            axum::routing::post(cancel),
        )
        .route("/api/organizations/{id}/teams/{team}", get(own))
        .route(
            "/api/organizations/{id}/teams/{team}/repositories",
            get(repositories),
        )
        .route(
            "/api/organizations/{id}/teams/{team}/identity-group",
            get(identity_group).put(link_identity_group),
        )
        .route("/api/organizations/{id}/teams/{team}/members", get(members))
        .route(
            "/api/organizations/{id}/teams/{team}/candidates",
            get(candidates),
        )
        .route(
            "/api/organizations/{id}/teams/{team}/members/{person}",
            get(membership),
        )
        .route(
            "/api/organizations/{id}/teams/{team}/review-routing",
            get(review_routing),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HierarchyParameters {
    team: Option<Uuid>,
    after: Option<Uuid>,
    request: Option<Uuid>,
    limit: Option<u8>,
}

async fn hierarchy_requests(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(organization): Path<Uuid>,
    Query(params): Query<HierarchyParameters>,
) -> Result<Json<native::hierarchy_requests::Page>> {
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid(
            "Choose a team request page size from 1 to 50",
        ));
    }
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = cells
        .query::<native::hierarchy_requests::List>(
            target(organization)?,
            None,
            native::hierarchy_requests::ListInput {
                actor,
                team: params.team,
                after: params.after,
                request: params.request,
                incoming: false,
                limit,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(result))
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
    search: String,
    parent: Option<Uuid>,
}
impl Parameters {
    fn limit(&self) -> Result<u8> {
        let limit = self.limit.unwrap_or(30);
        if !(1..=50).contains(&limit) {
            return Err(Error::Invalid("Choose a team page size from 1 to 50"));
        }
        Ok(limit)
    }
}
fn decode<T: DeserializeOwned>(cursor: Option<&str>) -> Result<Option<T>> {
    cursor
        .map(|value| {
            if value.len() > 4096 {
                return Err(Error::Invalid("Team page cursor is invalid"));
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(value)
                .map_err(|_| Error::Invalid("Team page cursor is invalid"))?;
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::Invalid("Team page cursor is invalid"))
        })
        .transpose()
}
fn encode<T: serde::Serialize>(cursor: Option<T>) -> Result<Option<String>> {
    cursor
        .map(|value| {
            serde_json::to_vec(&value)
                .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
                .map_err(Error::from)
        })
        .transpose()
}
async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<Parameters>,
) -> Result<Json<TeamListing>> {
    if params.search.chars().count() > 100 || params.search.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Team search must contain at most 100 characters without control characters",
        ));
    }
    let limit = params.limit()?;
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = cells
        .query::<native::List>(
            target(id)?,
            None,
            native::ListInput {
                actor,
                search: params.search,
                parent: params.parent,
                after: decode::<native::QueryCursor>(params.after.as_deref())?,
                limit,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    let native::Page::Page {
        items,
        next,
        revision,
    } = result.page
    else {
        return Err(Error::PageChanged);
    };
    app::account(&principal)?;
    Ok(Json(TeamListing {
        page: super::OrganizationPage {
            items,
            next: encode(next)?,
            revision,
        },
        can_create: result.can_create,
    }))
}
async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
) -> Result<Json<native::View>> {
    Ok(Json(view(&server, &principal, id, team).await?))
}

async fn view(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    team: Uuid,
) -> Result<native::View> {
    let actor = actor(server, principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let view = cells
        .query::<native::Get>(target(id)?, None, native::Key { actor, team })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(principal)?;
    Ok(view)
}

async fn repositories(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((organization, team)): Path<(Uuid, Uuid)>,
    Query(search): Query<Search>,
) -> Result<Json<serde_json::Value>> {
    let before = view(&server, &principal, organization, team).await?;
    let page = catalog_browse::list(
        &server,
        &principal,
        Scope::Team {
            organization,
            team,
            ancestors: before.ancestors.clone().unwrap_or_default(),
        },
        search,
    )
    .await?;
    // A team can become secret or be deleted while the catalog page is read.
    if view(&server, &principal, organization, team).await? != before {
        return Err(Error::PageChanged);
    }
    Ok(page)
}
#[derive(Serialize)]
struct IdentityGroup {
    id: Uuid,
    name: String,
}

#[derive(Serialize)]
struct IdentityGroupStatus {
    group: Option<IdentityGroup>,
    version: u64,
    enabled: bool,
    can_link: bool,
}

async fn group_status(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    team: Uuid,
) -> Result<(Uuid, native::sync::Status)> {
    let actor = actor(server, principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let value = cells
        .query::<native::sync::GetSync>(target(id)?, None, native::Key { actor, team })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(principal)?;
    Ok((actor, value))
}

async fn identity_group(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
) -> Result<Json<IdentityGroupStatus>> {
    let (_, state) = group_status(&server, &principal, id, team).await?;
    Ok(Json(IdentityGroupStatus {
        group: state.binding.map(|binding| IdentityGroup {
            id: binding.group,
            name: binding.name,
        }),
        version: state.version,
        enabled: crate::auth::scim::groups::enabled(&server),
        can_link: state.can_link,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkIdentityGroup {
    version: u64,
    group: Option<Uuid>,
}

async fn link_identity_group(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
    Json(input): Json<LinkIdentityGroup>,
) -> Result<Json<serde_json::Value>> {
    let (actor, state) = group_status(&server, &principal, id, team).await?;
    if !state.can_link {
        return Err(Error::NotFound);
    }
    crate::auth::scim::groups::configure_team(&server, id, team, actor, input.version, input.group)
        .await?;
    Ok(Json(serde_json::json!({"applied": true})))
}

async fn policy(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<native::Policy>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let policy = cells
        .query::<native::GetPolicy>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(policy))
}
async fn review_routing(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
) -> Result<Json<native::review_routing::Settings>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let settings = cells
        .query::<native::review_routing::GetSettings>(
            target(id)?,
            None,
            native::Key { actor, team },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(settings))
}

enum People {
    Members,
    Candidates,
}
async fn people(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    team: Uuid,
    params: Parameters,
    kind: People,
) -> Result<Json<TeamRoster>> {
    if !params.search.is_empty() || params.parent.is_some() {
        return Err(Error::Invalid(
            "Team member pages do not accept a search filter",
        ));
    }
    let actor = actor(server, principal).await?;
    let target = target(id)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let input = native::MemberInput {
        actor,
        team,
        after: decode::<native::MemberCursor>(params.after.as_deref())?,
        limit: params.limit()?,
    };
    let roster = matches!(kind, People::Members);
    let page = match kind {
        People::Members => cells.query::<native::Members>(target, None, input).await,
        People::Candidates => cells.query::<native::Candidates>(target, None, input).await,
    }
    .map_err(read_error)?
    .output;
    let native::Page::Page {
        items,
        next,
        revision,
    } = page
    else {
        return Err(Error::PageChanged);
    };
    let profiles = app::query_output(
        cells
            .query::<LookupProfiles>(
                Address::Directory,
                None,
                items.iter().map(|member| member.actor).collect(),
            )
            .await,
    )?;
    if profiles.len() != items.len() {
        return Err(Error::CellContract(
            "Team profile lookup returned an invalid result count",
        ));
    }
    // Directory profiles are public, but their team association is private.
    // Reauthorize in the team owner and require the same roster revision after enrichment.
    let current = cells
        .query::<native::sync::GetSync>(target, None, native::Key { actor, team })
        .await
        .map_err(read_error)?;
    let current_view = current.output.ok_or(Error::NotFound)?;
    if current.receipt.revision != revision {
        return Err(Error::PageChanged);
    }
    app::account(principal)?;
    let items = items
        .into_iter()
        .zip(profiles)
        .map(|(membership, profile)| TeamMember {
            actor: membership.actor,
            role: membership.role,
            version: membership.version,
            profile,
            inherited: (roster && membership.version == 0).then_some(true),
        })
        .collect();
    Ok(Json(TeamRoster {
        page: super::OrganizationPage {
            items,
            next: encode(next)?,
            revision,
        },
        can_manage: current_view.can_manage && current_view.binding.is_none(),
    }))
}
async fn members(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
    Query(params): Query<Parameters>,
) -> Result<Json<TeamRoster>> {
    people(&server, &principal, id, team, params, People::Members).await
}
async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
    Query(params): Query<Parameters>,
) -> Result<Json<TeamRoster>> {
    people(&server, &principal, id, team, params, People::Candidates).await
}
async fn membership(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team, person)): Path<(Uuid, Uuid, Uuid)>,
) -> Result<Json<native::Membership>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let member = cells
        .query::<native::GetMembership>(
            target(id)?,
            None,
            native::MemberKey {
                actor,
                team,
                member: person,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(member))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "TeamSubmission"))]
pub(in crate::api) struct ChangeRequest {
    request_id: String,
    command: native::Command,
}
enum Submission {
    Apply,
    Cancel,
}
async fn save(
    server: &Arc<Server>,
    principal: &Principal,
    id: Uuid,
    input: ChangeRequest,
    submission: Submission,
) -> Result<Response> {
    let request_id = match app::submission_id(&input.request_id) {
        Ok(id) => Uuid::from_bytes(id),
        Err(error) => return Ok(write::rejected(error)),
    };
    let actor = actor(server, principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::account(principal)?;
    let creating = matches!(input.command, native::Command::Create { .. })
        && matches!(submission, Submission::Apply);
    let request = native::Request {
        actor,
        request_id,
        command: input.command,
    };
    let address = target(id)?;
    let outcome = match submission {
        Submission::Apply => {
            let principal = principal.clone();
            super::with_org_policy(server, id, move |_| async move {
                app::account(&principal)?;
                app::command_output(
                    cells
                        .command::<native::Change>(address, app::mutation_identity()?, request)
                        .await,
                )
            })
            .await?
        }
        Submission::Cancel => app::command_output(
            cells
                .command::<native::Cancel>(address, app::mutation_identity()?, request)
                .await,
        )?,
    };
    let error = match outcome {
        native::Outcome::Applied { id } => {
            return Ok((
                if creating {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                Json(Acknowledgement::Applied { id }),
            )
                .into_response());
        }
        native::Outcome::Cancelled { id } => {
            return Ok(Json(Acknowledgement::Cancelled { id }).into_response());
        }
        native::Outcome::Conflict => Error::Conflict,
        native::Outcome::InvalidName => Error::Invalid(
            "Team name must contain 1–100 characters, without control characters or surrounding whitespace",
        ),
        native::Outcome::InvalidDescription => Error::Invalid(
            "Team description must contain at most 1000 characters, without NUL characters",
        ),
        native::Outcome::InvalidParent => {
            Error::Invalid("Choose a visible parent team outside this team's own hierarchy")
        }
        native::Outcome::NestedVisibility => {
            Error::Invalid("Remove the team from its hierarchy before making it secret")
        }
        native::Outcome::InvalidRouting => Error::Invalid("Choose between 1 and 10 reviewers"),
        native::Outcome::InvalidRoutingExclusions => {
            Error::Invalid("Choose up to 256 current team members to exclude")
        }
        native::Outcome::NameTaken => Error::TeamNameConflict,
        native::Outcome::Missing => Error::NotFound,
        native::Outcome::NotMember => Error::Invalid(
            "Only current organization members can join a team. Invite them to the organization first",
        ),
        native::Outcome::SubmissionConflict => return Err(Error::RequestConflict),
    };
    Ok(write::rejected(error))
}
async fn apply(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<ChangeRequest>,
) -> Result<Response> {
    save(&server, &principal, id, input, Submission::Apply).await
}
async fn cancel(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<ChangeRequest>,
) -> Result<Response> {
    save(&server, &principal, id, input, Submission::Cancel).await
}

#[cfg(test)]
mod tests;
