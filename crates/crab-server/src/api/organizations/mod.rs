use std::{future::Future, sync::Arc};
use tokio_util::sync::CancellationToken;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address, CellClient,
        directory::ActorSummary,
        organization::{
            self, Caller, ChangeMember, ChangeOutcome, Collection, Edit, EditInput, FindMembers,
            GetOrganization, MemberChange, Role,
        },
    },
    repository::settings::{RulesetScope, with_ruleset_lease},
    server::Server,
};

pub(super) mod audit;
pub(super) mod automation;
pub(super) mod collaborators;
mod discovery;
pub(super) mod invitations;
pub(super) mod issue_types;
pub(super) mod membership_changes;
mod people;
mod projects;
pub(super) mod properties;
pub(super) mod repository_access;
mod repository_policy;
mod rulesets;
pub(super) mod teams;

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct OrganizationView {
    organization: organization::Organization,
    actor: Uuid,
    role: Role,
    can_create: bool,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct OrganizationPage<T> {
    items: Vec<T>,
    next: Option<String>,
    revision: u64,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct OrganizationMembership {
    actor: Uuid,
    role: Role,
    version: u64,
    profile: Option<ActorSummary>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct OrganizationMembers {
    #[serde(flatten)]
    page: OrganizationPage<OrganizationMembership>,
    can_manage: bool,
}

#[cfg(test)]
mod tests;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .merge(invitations::routes())
        .merge(collaborators::routes())
        .merge(membership_changes::routes())
        .merge(audit::routes())
        .merge(automation::routes())
        .merge(repository_access::routes())
        .merge(repository_policy::routes())
        .merge(teams::routes())
        .route("/api/organizations", get(discovery::list))
        .route("/api/organizations/{id}", get(own).patch(edit))
        .route("/api/organizations/{id}/members", get(members))
        .route(
            "/api/organizations/{id}/members/{actor_id}",
            get(member).put(save_member).delete(remove_member),
        )
        .layer(axum::extract::DefaultBodyLimit::max(8192))
        .merge(projects::routes())
        .merge(rulesets::routes())
        .merge(properties::organization_routes())
        .merge(issue_types::routes().layer(axum::extract::DefaultBodyLimit::max(32 * 1024)))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
pub(in crate::api) async fn actor(server: &Server, principal: &Principal) -> Result<Uuid> {
    let identity = app::account(principal)?;
    let actor = crate::repository::access::actor_id(server, identity).await?;
    app::account(principal)?;
    actor.ok_or(Error::NotFound)
}
pub(super) fn read_error(error: crate::cells::Error) -> Error {
    match error {
        crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied => Error::NotFound,
        other => Error::Cell(other),
    }
}
async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<OrganizationView>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let landing = cells
        .query::<organization::discovery::GetLanding>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output;
    let Some(organization::discovery::Landing::Member {
        view, can_create, ..
    }) = landing
    else {
        return Err(Error::NotFound);
    };
    app::account(&principal)?;
    Ok(Json(OrganizationView {
        organization: view.organization,
        role: view.role,
        can_create,
        actor,
    }))
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
async fn members(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<people::Parameters>,
) -> Result<Json<OrganizationMembers>> {
    let (page, role) = people::page::<FindMembers, _, _>(
        &server,
        &principal,
        id,
        Collection::Members,
        params,
        |member| member.actor,
        |member, profile| OrganizationMembership {
            actor: member.actor,
            role: member.role,
            version: member.version,
            profile,
        },
    )
    .await?;
    Ok(Json(OrganizationMembers {
        page,
        can_manage: role == Role::Owner,
    }))
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
        principal,
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
    let removed = membership_changes::depart(
        &server,
        principal,
        id,
        vec![organization::DepartureMember {
            actor: member,
            version: input.version,
        }],
        organization::DepartureMode::Remove,
    )
    .await?;
    let [member] = removed.as_slice() else {
        return Err(Error::CellContract(
            "Expected one removed organization member",
        ));
    };
    Ok(Json(serde_json::to_value(member)?))
}
async fn change_member(
    server: &Arc<Server>,
    principal: Principal,
    id: Uuid,
    member: Uuid,
    version: u64,
    role: Option<Role>,
) -> Result<Json<Value>> {
    let actor = actor(server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let address = target(id)?;
    let value = with_org_policy(server, id, move |_| async move {
        app::account(&principal)?;
        app::command_output(
            cells
                .command::<ChangeMember>(
                    address,
                    app::mutation_identity()?,
                    MemberChange {
                        actor,
                        member,
                        version,
                        role,
                    },
                )
                .await,
        )
    })
    .await?;
    Ok(Json(serde_json::to_value(applied(value)?)?))
}

pub(super) async fn with_org_policy<T, F, Fut>(server: &Arc<Server>, id: Uuid, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + Send,
{
    // Settle pending membership changes before reusing this organization's
    // authority for another membership change, team command, or reservation.
    let worker = Arc::clone(server);
    server
        .receives
        .spawn(async move {
            with_ruleset_lease(
                &worker,
                RulesetScope::Organization(id),
                &worker.cancellation,
                |cancel| async {
                    if cancel.is_cancelled() {
                        return Err(Error::MergeBusy);
                    }
                    membership_changes::recover_locked(&worker, id).await?;
                    if cancel.is_cancelled() {
                        return Err(Error::MergeBusy);
                    }
                    work(cancel).await
                },
            )
            .await
        })
        .await
        .map_err(|_| Error::MergeBusy)?
}
fn target(id: Uuid) -> Result<Address> {
    if id.is_nil() {
        return Err(Error::Invalid("Organization ID is invalid"));
    }
    Ok(Address::Organization(id))
}

async fn require_owner(cells: &CellClient, address: Address, owner: Uuid) -> Result<()> {
    let role = cells
        .query::<GetOrganization>(address, None, Caller { actor: owner })
        .await
        .map_err(read_error)?
        .output
        .map(|result| result.role);
    if role != Some(Role::Owner) {
        return Err(Error::NotFound);
    }
    Ok(())
}
