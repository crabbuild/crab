use crate::{
    app::{self, Error, Result},
    cells::repository::issues::lifecycle::{
        GetIssueLifecycleStatus, IssueLifecycleStatus, PrepareIssueLifecycle,
        PrepareIssueLifecycleInput, PrepareIssueLifecycleOutcome,
    },
    server::Server,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/internal/repositories/{owner}/{name}/issue-state",
            get(status).post(prepare),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024))
}
async fn status(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
) -> Result<Json<IssueLifecycleStatus>> {
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    Ok(Json(app::query_output(
        cell.client
            .query::<GetIssueLifecycleStatus>(&cell.target, None, ())
            .await,
    )?))
}
async fn prepare(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
    Json(input): Json<PrepareIssueLifecycleInput>,
) -> Result<Json<IssueLifecycleStatus>> {
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    match app::command_output(
        cell.client
            .command::<PrepareIssueLifecycle>(&cell.target, app::mutation_identity()?, input)
            .await,
    )? {
        PrepareIssueLifecycleOutcome::Applied(status) => Ok(Json(status)),
        PrepareIssueLifecycleOutcome::Conflict(_) => Err(Error::Conflict),
    }
}
