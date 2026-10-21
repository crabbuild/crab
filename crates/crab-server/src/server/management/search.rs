use std::sync::Arc;

use crate::cells::repository::search::index::{
    GetSearchIndexStatus, RebuildIndex, RebuildSearchInput, RebuildSearchOutcome,
};
use crate::{app, server::Server};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/internal/repositories/{owner}/{name}/discussion-index",
            get(status).post(rebuild),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024))
}

async fn status(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
) -> app::Result<Response> {
    let repository = server.repositories.get(&key).ok_or(app::Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    let status = app::query_output(
        cell.client
            .query::<GetSearchIndexStatus>(&cell.target, None, ())
            .await,
    )?;
    Ok(Json(status).into_response())
}

async fn rebuild(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
    Json(input): Json<RebuildSearchInput>,
) -> app::Result<Response> {
    if server.cancellation.is_cancelled() {
        return Ok(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| app::Error::CellUnavailable)?;
    let repository = server.repositories.get(&key).ok_or(app::Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    let result = app::command_output(
        cell.client
            .command::<RebuildIndex>(&cell.target, app::mutation_identity()?, input)
            .await,
    )?;
    Ok(match result {
        RebuildSearchOutcome::Progress { status } => Json(status).into_response(),
        RebuildSearchOutcome::Conflict { status } => {
            (StatusCode::CONFLICT, Json(status)).into_response()
        }
    })
}
