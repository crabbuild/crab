use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
        organization::{self, Bootstrap, BootstrapInput, Caller, ChangeOutcome, teams},
    },
    server::Server,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route("/internal/organizations/{id}", post(bootstrap))
        .route(
            "/internal/organizations/{id}/teams",
            get(team_state).post(prepare_teams),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    owner: IdentityKey,
    name: String,
    #[serde(default)]
    description: String,
}
async fn bootstrap(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    if id.is_nil() {
        return Err(Error::Invalid("Organization ID is invalid"));
    }
    input.owner.validate().map_err(Error::Cell)?;
    organization::details(&input.name, &input.description).map_err(Error::Cell)?;
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    // Bootstrap is explicit private operator work. The qualified account is
    // registered without a profile; no catalog owner string acquires authority.
    let registered = app::command_output(
        cells
            .command::<RegisterAccounts>(
                Address::Directory,
                app::mutation_identity()?,
                vec![input.owner],
            )
            .await,
    )?;
    let [actor] = registered.as_slice() else {
        return Err(Error::CellContract(
            "Account registration returned an invalid result count",
        ));
    };
    let target = Address::Organization(id);
    cells.initialize(target).await.map_err(Error::Cell)?;
    let result = app::command_output(
        cells
            .command::<Bootstrap>(
                target,
                app::mutation_identity()?,
                BootstrapInput {
                    actor: *actor,
                    name: input.name,
                    description: input.description,
                },
            )
            .await,
    )?;
    match result {
        ChangeOutcome::Applied(organization) => Ok(Json(
            json!({"organization":organization,"owner":actor,"url":format!("/-/organizations/{id}")}),
        )),
        ChangeOutcome::Conflict => Err(Error::Conflict),
        _ => Err(Error::CellContract(
            "Organization bootstrap returned an invalid outcome",
        )),
    }
}

async fn team_state(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Query(input): Query<Caller>,
) -> Result<Json<teams::OrganizationTeamState>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let state = app::query_output(
        cells
            .query::<teams::GetPreparation>(Address::Organization(id), None, input)
            .await,
    )?;
    Ok(Json(state))
}
async fn prepare_teams(
    State(server): State<Arc<Server>>,
    Path(id): Path<Uuid>,
    Json(input): Json<teams::PrepareInput>,
) -> Result<Json<teams::OrganizationTeamState>> {
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let state = app::command_output(
        cells
            .command::<teams::Prepare>(Address::Organization(id), app::mutation_identity()?, input)
            .await,
    )?;
    Ok(Json(state))
}
