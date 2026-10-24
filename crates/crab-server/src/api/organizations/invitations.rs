use super::*;
use crate::cells::directory::{AccountsExist, CompleteHandles, validate_handle_prefix};
use organization::{
    CancelInvitation, DecideInvitation, Decision, GetInvitation, InvitationKey, Invite,
    InviteInput, ListInvitations,
};

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
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    read(&server, &principal, id, actor, actor).await
}
async fn offer(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, recipient)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    read(&server, &principal, id, actor, recipient).await
}
async fn read(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    actor: Uuid,
    recipient: Uuid,
) -> Result<Json<Value>> {
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
    Ok(Json(serde_json::to_value(view)?))
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
    Query(params): Query<MemberParameters>,
) -> Result<Json<Value>> {
    let actor = actor(&server, &principal).await?;
    let after = params.after.as_deref().map(decode).transpose()?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let page = cells
        .query::<ListInvitations>(
            target(id)?,
            None,
            PageQuery {
                actor,
                after,
                limit: params.limit.unwrap_or(30),
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
                items.iter().map(|value| value.recipient).collect(),
            )
            .await,
    )?;
    if profiles.len() != items.len() {
        return Err(Error::CellContract(
            "Invitation profiles returned an invalid result count",
        ));
    }
    // ListInvitations checks owner authority again after the Directory read. A
    // plain member can read the roster, but must not recover pending offers.
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
    let items = items
        .into_iter()
        .zip(profiles)
        .map(|(value, profile)| json!({"invitation":value,"profile":profile}))
        .collect::<Vec<_>>();
    let next = next
        .map(|cursor| serde_json::to_vec(&cursor).map(|bytes| URL_SAFE_NO_PAD.encode(bytes)))
        .transpose()?;
    Ok(Json(json!({"items":items,"next":next,"revision":revision})))
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
