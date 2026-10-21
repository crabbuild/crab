use crate::{
    app,
    cells::{Address, directory::GetDirectoryRevision},
    server::Server,
};
use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route("/internal/directory", get(status).post(initialize))
}
async fn status(State(server): State<Arc<Server>>) -> app::Result<Json<Value>> {
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    match cells
        .query::<GetDirectoryRevision>(Address::Directory, None, ())
        .await
    {
        Ok(observed) => Ok(Json(json!({"state":"ready","revision":observed.output}))),
        Err(crate::cells::Error::NotInitialized) => Ok(Json(json!({"state":"missing"}))),
        Err(error) => Err(app::Error::Cell(error)),
    }
}
async fn initialize(State(server): State<Arc<Server>>) -> app::Result<impl IntoResponse> {
    if server.cancellation.is_cancelled() {
        return Err(app::Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| app::Error::CellUnavailable)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    cells
        .initialize(Address::Directory)
        .await
        .map_err(app::Error::Cell)?;
    status(State(Arc::clone(&server))).await
}
