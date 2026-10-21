use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{
            self, ActorProfile, CreateProfile, CreateProfileInput, EditProfile, EditProfileInput,
            GetActor, GetOwnProfile, IdentityKey, ProfileOutcome,
        },
    },
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/profile", get(own).post(create).patch(edit))
        .route("/api/actors/{id}", get(actor))
        .layer(axum::extract::DefaultBodyLimit::max(2048))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn account_identity(principal: &Principal) -> Result<IdentityKey> {
    // Git tokens are repository-scoped credentials, even when their issuing
    // account can administer a repository. They cannot manage account identity.
    if !matches!(principal, Principal::User(_) | Principal::Local) {
        return Err(app::Error::SessionRequired);
    }
    let actor = principal.identity().ok_or(app::Error::SessionRequired)?;
    Ok(IdentityKey {
        issuer: actor.issuer,
        subject: actor.subject,
    })
}

async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Value>> {
    let identity = account_identity(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let profile = app::query_output(
        cells
            .query::<GetOwnProfile>(Address::Directory, None, identity)
            .await,
    )?;
    account_identity(&principal)?;
    Ok(Json(json!({"profile":profile})))
}
async fn actor(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<ActorProfile>> {
    account_identity(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let profile = app::query_output(cells.query::<GetActor>(Address::Directory, None, id).await)?
        .ok_or(app::Error::NotFound)?;
    account_identity(&principal)?;
    Ok(Json(profile))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProfile {
    request_id: Uuid,
    handle: String,
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileEdit {
    version: u64,
    handle: String,
    name: String,
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<NewProfile>, JsonRejection>,
) -> Result<(StatusCode, Json<ActorProfile>)> {
    let identity = account_identity(&principal)?;
    let Json(input) = input.map_err(app::Error::Body)?;
    directory::validate_profile(&input.handle, &input.name).map_err(app::Error::Invalid)?;
    if input.request_id.is_nil() {
        return Err(app::Error::Invalid("Profile submission ID is invalid"));
    }
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let input = CreateProfileInput {
        submission_id: input.request_id,
        identity,
        handle: input.handle,
        name: input.name,
    };
    let outcome = app::command_output(
        cells
            .command::<CreateProfile>(Address::Directory, app::mutation_identity()?, input)
            .await,
    )?;
    account_identity(&principal)?;
    Ok((StatusCode::CREATED, Json(saved(outcome)?)))
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<ProfileEdit>, JsonRejection>,
) -> Result<Json<ActorProfile>> {
    let identity = account_identity(&principal)?;
    let Json(input) = input.map_err(app::Error::Body)?;
    directory::validate_profile(&input.handle, &input.name).map_err(app::Error::Invalid)?;
    if !(1..=app::MAX_NUMBER).contains(&input.version) {
        return Err(app::Error::Invalid("Profile version is invalid"));
    }
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let input = EditProfileInput {
        identity,
        version: input.version,
        handle: input.handle,
        name: input.name,
    };
    let outcome = app::command_output(
        cells
            .command::<EditProfile>(Address::Directory, app::mutation_identity()?, input)
            .await,
    )?;
    account_identity(&principal)?;
    Ok(Json(saved(outcome)?))
}
fn saved(outcome: ProfileOutcome) -> Result<ActorProfile> {
    match outcome {
        ProfileOutcome::Saved(profile) => Ok(profile),
        ProfileOutcome::HandleTaken => Err(app::Error::ProfileHandleTaken),
        ProfileOutcome::AlreadyRegistered => Err(app::Error::ProfileExists),
        ProfileOutcome::Conflict => Err(app::Error::Conflict),
        ProfileOutcome::RequestConflict => Err(app::Error::RequestConflict),
        ProfileOutcome::NotFound => Err(app::Error::NotFound),
    }
}

#[cfg(test)]
mod tests;
