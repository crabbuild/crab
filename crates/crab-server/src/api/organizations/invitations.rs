use super::*;
use crate::cells::directory::{AccountsExist, CompleteHandles, validate_handle_prefix};
use organization::{
    CancelInvitation, DecideInvitation, Decision, FindInvitations, GetInvitation, InvitationKey,
    Invite, InviteInput,
};

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(in crate::api) struct InvitationOffer {
    invitation: organization::Invitation,
    profile: Option<ActorSummary>,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/api/organizations/{id}/invitations", get(list))
        .route(
            "/api/organizations/{id}/invitations/candidates",
            get(candidates),
        )
        .route(
            "/api/organizations/{id}/invitations/{recipient}",
            get(offer).put(invite).delete(cancel),
        )
        .route("/api/organizations/{id}/invitation", get(own).post(decide))
}
async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<organization::InvitationView>> {
    let actor = actor(&server, &principal).await?;
    read(&server, &principal, id, actor, actor).await
}
async fn offer(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, recipient)): Path<(Uuid, Uuid)>,
) -> Result<Json<organization::InvitationView>> {
    let actor = actor(&server, &principal).await?;
    read(&server, &principal, id, actor, recipient).await
}
async fn read(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    actor: Uuid,
    recipient: Uuid,
) -> Result<Json<organization::InvitationView>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let view = cells
        .query::<GetInvitation>(
            target(id)?,
            None,
            organization::MemberKey {
                actor,
                member: recipient,
            },
        )
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    app::account(principal)?;
    Ok(Json(view))
}
async fn invite(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, recipient)): Path<(Uuid, Uuid)>,
    Json(input): Json<MemberRequest>,
) -> Result<Json<organization::Invitation>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let current = cells
        .query::<GetOrganization>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    if current.role != Role::Owner {
        return Err(Error::Cell(crate::cells::Error::AccessDenied));
    }
    // Only a current owner may probe private account registration. The command
    // checks owner authority again after this separate Directory await.
    let exists = app::query_output(
        cells
            .query::<AccountsExist>(Address::Directory, None, vec![recipient])
            .await,
    )?;
    if exists.as_slice() != [true] {
        return Err(Error::Invalid("Choose a registered account"));
    }
    // Record the candidate before the offer can commit. Discovery and mail
    // disclose only offers that the Organization Cell currently confirms.
    super::discovery::record_candidate(&cells, id, recipient).await?;
    app::account(&principal)?;
    let result = app::command_output(
        cells
            .command::<Invite>(
                target(id)?,
                app::mutation_identity()?,
                InviteInput {
                    actor,
                    recipient,
                    version: input.version,
                    role: input.role,
                },
            )
            .await,
    )?;
    Ok(Json(applied(result)?))
}
async fn cancel(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, recipient)): Path<(Uuid, Uuid)>,
    Json(input): Json<Version>,
) -> Result<Json<organization::Invitation>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::command_output(
        cells
            .command::<CancelInvitation>(
                target(id)?,
                app::mutation_identity()?,
                InvitationKey {
                    actor,
                    recipient,
                    version: input.version,
                },
            )
            .await,
    )?;
    Ok(Json(applied(result)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    version: u64,
    accept: bool,
}
async fn decide(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Json(input): Json<Answer>,
) -> Result<Json<organization::Invitation>> {
    let actor = actor(&server, &principal).await?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::command_output(
        cells
            .command::<DecideInvitation>(
                target(id)?,
                app::mutation_identity()?,
                Decision {
                    actor,
                    version: input.version,
                    accept: input.accept,
                },
            )
            .await,
    )?;
    Ok(Json(applied(result)?))
}
async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(params): Query<people::Parameters>,
) -> Result<Json<OrganizationPage<InvitationOffer>>> {
    let (page, _) = people::page::<FindInvitations, _, _>(
        &server,
        &principal,
        id,
        Collection::Invitations,
        params,
        |invitation| invitation.recipient,
        |invitation, profile| InvitationOffer {
            invitation,
            profile,
        },
    )
    .await?;
    Ok(Json(page))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    q: String,
}
async fn candidates(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(input): Query<Search>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let prefix = input.q.trim().trim_start_matches('@').to_ascii_lowercase();
    validate_handle_prefix(&prefix).map_err(Error::Invalid)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::query_output(
        cells
            .query::<CompleteHandles>(Address::Directory, None, prefix)
            .await,
    )?;
    let current = cells
        .query::<GetOrganization>(target(id)?, None, Caller { actor })
        .await
        .map_err(read_error)?
        .output
        .ok_or(Error::NotFound)?;
    if current.role != Role::Owner {
        return Err(Error::NotFound);
    }
    app::account(&principal)?;
    Ok(Json(serde_json::to_value(result)?))
}
