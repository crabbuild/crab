use super::{actor, read_error, target};
use crate::{
    api::write,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::LookupProfiles,
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
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/teams", get(list))
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
        .route("/api/organizations/{id}/teams/{team}/members", get(members))
        .route(
            "/api/organizations/{id}/teams/{team}/candidates",
            get(candidates),
        )
        .route(
            "/api/organizations/{id}/teams/{team}/members/{person}",
            get(membership),
        )
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Parameters {
    after: Option<String>,
    limit: Option<u8>,
    search: String,
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
) -> Result<Json<Value>> {
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
    Ok(Json(
        json!({"items":items,"next":encode(next)?,"revision":revision,"can_create":result.can_create}),
    ))
}
async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
) -> Result<Json<native::View>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let view = cells
        .query::<native::Get>(target(id)?, None, native::Key { actor, team })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(&principal)?;
    Ok(Json(view))
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
) -> Result<Json<Value>> {
    if !params.search.is_empty() {
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
        .query::<native::Get>(target, None, native::Key { actor, team })
        .await
        .map_err(read_error)?;
    let current_view = current.output.ok_or(Error::NotFound)?;
    if current.receipt.revision != revision {
        return Err(Error::PageChanged);
    }
    app::account(principal)?;
    let items=items.into_iter().zip(profiles).map(|(membership,profile)|json!({"actor":membership.actor,"role":membership.role,"version":membership.version,"profile":profile})).collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"next":encode(next)?,"revision":revision,"can_manage":current_view.can_manage}),
    ))
}
async fn members(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    people(&server, &principal, id, team, params, People::Members).await
}
async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, team)): Path<(Uuid, Uuid)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
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
struct ChangeRequest {
    request_id: String,
    command: native::Command,
}
enum Submission {
    Apply,
    Cancel,
}
async fn save(
    server: &Server,
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
    let result = match submission {
        Submission::Apply => {
            cells
                .command::<native::Change>(target(id)?, app::mutation_identity()?, request)
                .await
        }
        Submission::Cancel => {
            cells
                .command::<native::Cancel>(target(id)?, app::mutation_identity()?, request)
                .await
        }
    };
    let outcome = app::command_output(result)?;
    let error = match outcome {
        native::Outcome::Applied { id } => {
            return Ok((
                if creating {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                Json(json!({"outcome":"applied","id":id})),
            )
                .into_response());
        }
        native::Outcome::Cancelled { id } => {
            return Ok(Json(json!({"outcome":"cancelled","id":id})).into_response());
        }
        native::Outcome::Conflict => Error::Conflict,
        native::Outcome::InvalidName => Error::Invalid(
            "Team name must contain 1–100 characters, without control characters or surrounding whitespace",
        ),
        native::Outcome::InvalidDescription => Error::Invalid(
            "Team description must contain at most 1000 characters, without NUL characters",
        ),
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
