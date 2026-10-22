use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{LookupAccounts, LookupProfiles},
        organization::{
            self, Caller, ChangeMember, ChangeOutcome, Edit, EditInput, GetOrganization,
            ListMembers, MemberChange, Page, PageCursor, PageQuery, Role,
        },
    },
    server::Server,
};

mod invitations;

#[cfg(test)]
mod tests;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .merge(invitations::routes())
        .route("/api/organizations/{id}", get(own).patch(edit))
        .route("/api/organizations/{id}/members", get(members))
        .route(
            "/api/organizations/{id}/members/{actor_id}",
            get(member).put(save_member).delete(remove_member),
        )
        .layer(axum::extract::DefaultBodyLimit::max(8192))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
async fn actor(server: &Server, principal: &Principal) -> Result<Uuid> {
    let identity = app::account(principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let ids = app::query_output(
        cells
            .query::<LookupAccounts>(Address::Directory, None, vec![identity])
            .await,
    )?;
    app::account(principal)?;
    match ids.as_slice() {
        [Some(id)] => Ok(*id),
        [None] => Err(Error::NotFound),
        _ => Err(Error::CellContract(
            "Account lookup returned an invalid result count",
        )),
    }
}
fn read_error(error: crate::cells::Error) -> Error {
    match error {
        crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied => Error::NotFound,
        other => Error::Cell(other),
    }
}
async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let view = cells
        .query::<GetOrganization>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(&principal)?;
    Ok(Json(
        json!({"organization":view.organization,"role":view.role,"actor":actor}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditRequest {
    version: u64,
    name: String,
    description: String,
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<EditRequest>,
) -> Result<Json<organization::Organization>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::account(&principal)?;
    let result = app::command_output(
        cells
            .command::<Edit>(
                target(id)?,
                app::mutation_identity()?,
                EditInput {
                    actor,
                    version: input.version,
                    name: input.name,
                    description: input.description,
                },
            )
            .await,
    )?;
    Ok(Json(applied(result)?))
}
fn applied<T>(outcome: ChangeOutcome<T>) -> Result<T> {
    match outcome {
        ChangeOutcome::Applied(value) => Ok(value),
        ChangeOutcome::Conflict => Err(Error::Conflict),
        ChangeOutcome::NotFound => Err(Error::NotFound),
        ChangeOutcome::LastOwner => Err(Error::Invalid(
            "An organization must retain at least one owner. Invite or promote another owner first",
        )),
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MemberParameters {
    after: Option<String>,
    limit: Option<u8>,
}
fn decode(value: &str) -> Result<PageCursor> {
    if value.len() > 512 {
        return Err(Error::Invalid("Organization member cursor is invalid"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| Error::Invalid("Organization member cursor is invalid"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Invalid("Organization member cursor is invalid"))
}
async fn members(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<MemberParameters>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let after = params.after.as_deref().map(decode).transpose()?;
    let limit = params.limit.unwrap_or(30);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid(
            "Choose an organization member page size from 1 to 50",
        ));
    }
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let page = cells
        .query::<ListMembers>(
            target(id)?,
            None,
            PageQuery {
                actor,
                after,
                limit,
            },
        )
        .await
        .map_err(read_error)?
        .output;
    let Page::Page {
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
            "Organization profile lookup returned an invalid result count",
        ));
    }
    // Public profile lookup is a separate authority. Recheck organization access
    // before returning its roster or management controls after that await.
    let current = cells
        .query::<GetOrganization>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(&principal)?;
    let items=items.into_iter().zip(profiles).map(|(member,profile)|json!({"actor":member.actor,"role":member.role,"version":member.version,"profile":profile})).collect::<Vec<_>>();
    let next = next
        .map(|cursor| serde_json::to_vec(&cursor).map(|bytes| URL_SAFE_NO_PAD.encode(bytes)))
        .transpose()?;
    Ok(Json(
        json!({"items":items,"next":next,"revision":revision,"can_manage":current.role==Role::Owner}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberRequest {
    version: u64,
    role: Role,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    version: u64,
}
async fn member(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, member)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let value = cells
        .query::<organization::GetMember>(
            target(id)?,
            None,
            organization::MemberKey { actor, member },
        )
        .await
        .map_err(read_error)?
        .output;
    app::account(&principal)?;
    Ok(Json(json!({"member":value})))
}
async fn save_member(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, member)): Path<(Uuid, Uuid)>,
    Json(input): Json<MemberRequest>,
) -> Result<Json<Value>> {
    change_member(
        &server,
        &principal,
        id,
        member,
        input.version,
        Some(input.role),
    )
    .await
}
async fn remove_member(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, member)): Path<(Uuid, Uuid)>,
    Json(input): Json<Version>,
) -> Result<Json<Value>> {
    change_member(&server, &principal, id, member, input.version, None).await
}
async fn change_member(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    member: Uuid,
    version: u64,
    role: Option<Role>,
) -> Result<Json<Value>> {
    let actor = actor(server, principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::account(principal)?;
    let value = app::command_output(
        cells
            .command::<ChangeMember>(
                target(id)?,
                app::mutation_identity()?,
                MemberChange {
                    actor,
                    member,
                    version,
                    role,
                },
            )
            .await,
    )?;
    Ok(Json(serde_json::to_value(applied(value)?)?))
}
fn target(id: Uuid) -> Result<Address> {
    if id.is_nil() {
        return Err(Error::Invalid("Organization ID is invalid"));
    }
    Ok(Address::Organization(id))
}
