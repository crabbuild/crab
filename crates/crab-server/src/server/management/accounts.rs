#![expect(
    clippy::result_large_err,
    reason = "handlers and their helpers return rejections as ready axum responses"
)]

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;

use crate::{app::Error, auth::AccountState, server::Server};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subject {
    subject: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    subject: String,
    expected_revision: u64,
    suspended: bool,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/internal/accounts/status", post(status))
        .route("/internal/accounts/state", post(change))
        .layer(DefaultBodyLimit::max(8192))
}

async fn status(
    State(server): State<Arc<Server>>,
    Json(input): Json<Subject>,
) -> Result<Json<AccountState>, Response> {
    let _permit = super::admit(&server).map_err(IntoResponse::into_response)?;
    let auth = server
        .auth
        .as_ref()
        .ok_or_else(|| Error::Invalid("Sign-in is not configured").into_response())?;
    auth.account_state(&input.subject)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn change(
    State(server): State<Arc<Server>>,
    Json(input): Json<Change>,
) -> Result<Json<AccountState>, Response> {
    let _permit = super::admit(&server).map_err(IntoResponse::into_response)?;
    let auth = server
        .auth
        .as_ref()
        .ok_or_else(|| Error::Invalid("Sign-in is not configured").into_response())?;
    auth.set_account_suspended(&input.subject, input.expected_revision, input.suspended)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}
