use crate::{
    app,
    cells::{
        Address,
        directory::{GetDirectoryState, IdentityKey, MigrateDirectory, RegisterAccounts},
    },
    server::Server,
};
use axum::{
    Json, Router,
    extract::State,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/internal/directory", get(status).post(initialize))
        .route("/internal/directory/migrate", post(migrate))
        .route("/internal/directory/accounts", post(register))
}
async fn status(State(server): State<Arc<Server>>) -> app::Result<Json<Value>> {
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    match cells
        .query::<GetDirectoryState>(Address::Directory, None, ())
        .await
    {
        Ok(observed) => Ok(Json(serde_json::to_value(observed.output)?)),
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

async fn migrate(State(server): State<Arc<Server>>) -> app::Result<Json<Value>> {
    if server.cancellation.is_cancelled() {
        return Err(app::Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| app::Error::CellUnavailable)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let result = app::command_output(
        cells
            .command::<MigrateDirectory>(Address::Directory, app::mutation_identity()?, ())
            .await,
    )?;
    Ok(Json(serde_json::to_value(result)?))
}

async fn register(
    State(server): State<Arc<Server>>,
    Json(identities): Json<Vec<IdentityKey>>,
) -> app::Result<Json<Value>> {
    if server.cancellation.is_cancelled() {
        return Err(app::Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| app::Error::CellUnavailable)?;
    let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
    let result = cells
        .command::<RegisterAccounts>(Address::Directory, app::mutation_identity()?, identities)
        .await
        .map_err(app::Error::Cell)?;
    Ok(Json(
        json!({"ids":result.output,"revision":result.receipt.revision}),
    ))
}
