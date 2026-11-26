use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::catalog_browse::{self, Scope, Search};
use crate::{
    app::{self, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{
            self, ActorProfile, CreateProfile, CreateProfileInput, EditProfile, EditProfileInput,
            GetActor, GetOwnProfile, LookupIdentities, ProfileOutcome,
            avatar::{AvatarConfig, GetOwnAvatar, SetAvatarInput, SetOwnAvatar},
        },
    },
    server::Server,
};

pub(in crate::api) mod attribution;
mod review_availability;
mod saved_replies;

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct OwnProfile {
    profile: Option<ActorProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    avatar: Option<AvatarConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AvatarUpdate {
    avatar: Option<AvatarConfig>,
}

#[derive(Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub(super) struct AvatarResponse {
    avatar: Option<AvatarConfig>,
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/profile", get(own).post(create).patch(edit))
        .route("/api/profile/avatar", get(get_avatar).put(put_avatar))
        .route("/api/actors/{id}", get(actor))
        .route("/api/actors/{id}/repositories", get(repositories))
        .route(
            "/api/profile/review-availability",
            get(review_availability::get).put(review_availability::put),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2048))
        .route("/api/profile/saved-replies", get(saved_replies::list))
        .route(
            "/api/profile/saved-replies/{id}",
            axum::routing::put(saved_replies::put)
                .delete(saved_replies::delete)
                .layer(axum::extract::DefaultBodyLimit::max(12 * 1024)),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn own(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<OwnProfile>> {
    let identity = app::account(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let profile = app::query_output(
        cells
            .query::<GetOwnProfile>(Address::Directory, None, identity.clone())
            .await,
    )?;
    let avatar = match &profile {
        Some(actor) => actor.avatar.clone(),
        None => app::query_output(
            cells
                .query::<GetOwnAvatar>(Address::Directory, None, identity)
                .await,
        )?,
    };
    app::account(&principal)?;
    Ok(Json(OwnProfile { profile, avatar }))
}

async fn get_avatar(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<AvatarResponse>> {
    let identity = app::account(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let avatar = app::query_output(
        cells
            .query::<GetOwnAvatar>(Address::Directory, None, identity)
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(AvatarResponse { avatar }))
}

async fn put_avatar(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<AvatarUpdate>, JsonRejection>,
) -> Result<Json<AvatarResponse>> {
    let identity = app::account(&principal)?;
    let Json(input) = input.map_err(app::Error::Body)?;
    if let Some(ref avatar) = input.avatar {
        avatar.validate().map_err(app::Error::Invalid)?;
    }
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let avatar = app::command_output(
        cells
            .command::<SetOwnAvatar>(
                Address::Directory,
                app::mutation_identity()?,
                SetAvatarInput {
                    identity,
                    avatar: input.avatar,
                },
            )
            .await,
    )?;
    app::account(&principal)?;
    Ok(Json(AvatarResponse { avatar }))
}
async fn actor(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<Json<ActorProfile>> {
    app::account(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let profile = app::query_output(cells.query::<GetActor>(Address::Directory, None, id).await)?
        .ok_or(app::Error::NotFound)?;
    app::account(&principal)?;
    Ok(Json(profile))
}

async fn repositories(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    Query(search): Query<Search>,
) -> Result<Json<serde_json::Value>> {
    let viewer = app::account(&principal)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    // Only profiles that already expose a public actor can be used here.
    app::query_output(cells.query::<GetActor>(Address::Directory, None, id).await)?
        .ok_or(app::Error::NotFound)?;
    let identities = app::query_output(
        cells
            .query::<LookupIdentities>(Address::Directory, None, vec![id])
            .await,
    )?;
    let [Some(identity)] = identities.as_slice() else {
        return Err(app::Error::CellContract(
            "Profile has no Directory identity",
        ));
    };
    let owner = if principal.is_local() && *identity == viewer {
        "local".into()
    } else {
        crate::catalog::personal_namespace(&identity.issuer, &identity.subject)
    };
    let page = catalog_browse::list(&server, &principal, Scope::Personal(owner), search).await?;
    app::account(&principal)?;
    Ok(page)
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
    let identity = app::account(&principal)?;
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
    app::account(&principal)?;
    Ok((StatusCode::CREATED, Json(saved(outcome)?)))
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    input: std::result::Result<Json<ProfileEdit>, JsonRejection>,
) -> Result<Json<ActorProfile>> {
    let identity = app::account(&principal)?;
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
    app::account(&principal)?;
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
