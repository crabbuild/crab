use crate::{
    app::{self, Error, Result},
    cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
        repository::{
            assignments::{
                Assignee, AssignmentBatch, AssignmentMapping, GetAssignmentBatch,
                MigrateAssignments, MigrateAssignmentsInput, MigrateAssignmentsOutcome,
                RecordMapping,
            },
            collaboration::Subject,
        },
    },
    server::Server,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use std::sync::Arc;

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/internal/repositories/{owner}/{name}/assignments",
            get(next).post(migrate),
        )
        .route(
            "/internal/repositories/{owner}/{name}/assignments/inventory",
            post(inventory),
        )
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
}
async fn next(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
) -> Result<Json<AssignmentBatch>> {
    inventory(State(server), Path(key), Json(None)).await
}
async fn inventory(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
    Json(after): Json<Option<Subject>>,
) -> Result<Json<AssignmentBatch>> {
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    Ok(Json(app::query_output(
        cell.client
            .query::<GetAssignmentBatch>(&cell.target, None, after)
            .await,
    )?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceMapping {
    subject: String,
    identity: IdentityKey,
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceRecord {
    target: Subject,
    version: u64,
    mappings: Vec<SourceMapping>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    records: Vec<SourceRecord>,
}
async fn migrate(
    State(server): State<Arc<Server>>,
    Path(key): Path<(String, String)>,
    Json(input): Json<Input>,
) -> Result<Response> {
    if server.cancellation.is_cancelled() {
        return Err(Error::CellUnavailable);
    }
    let _permit = server
        .maintenance_admission
        .try_acquire()
        .map_err(|_| Error::CellUnavailable)?;
    if input.records.len() > 8 || input.records.iter().any(|r| r.mappings.len() > 10) {
        return Err(Error::Invalid(
            "Assignment migration frame exceeds its record limit",
        ));
    }
    let repository = server.repositories.get(&key).ok_or(Error::NotFound)?;
    let cell = app::repository_cell(&server, &repository)?;
    let identities: Vec<_> = input
        .records
        .iter()
        .flat_map(|r| r.mappings.iter().map(|m| m.identity.clone()))
        .collect();
    let expected = identities.len();
    let registered = if identities.is_empty() {
        vec![]
    } else {
        app::command_output(
            cell.client
                .command::<RegisterAccounts>(
                    Address::Directory,
                    app::mutation_identity()?,
                    identities,
                )
                .await,
        )?
    };
    if registered.len() != expected {
        return Err(Error::CellContract(
            "Account registration returned an invalid result count",
        ));
    }
    let mut ids = registered.into_iter();
    let mut records = Vec::with_capacity(input.records.len());
    for record in input.records {
        let mut mappings = Vec::with_capacity(record.mappings.len());
        for mapped in record.mappings {
            let id = ids
                .next()
                .ok_or(Error::CellContract("Registered account is missing"))?;
            mappings.push(AssignmentMapping {
                subject: mapped.subject,
                assignee: Assignee {
                    id,
                    name: mapped.name,
                },
            });
        }
        records.push(RecordMapping {
            target: record.target,
            version: record.version,
            mappings,
        });
    }
    // The operator supplies historical qualification explicitly. Registration
    // never grants access; a conflicting repository frame may leave unused IDs.
    let outcome = app::command_output(
        cell.client
            .command::<MigrateAssignments>(
                &cell.target,
                app::mutation_identity()?,
                MigrateAssignmentsInput {
                    version: input.version,
                    records,
                },
            )
            .await,
    )?;
    match outcome {
        MigrateAssignmentsOutcome::Progress { state } => Ok(Json(state).into_response()),
        MigrateAssignmentsOutcome::Conflict { .. } => Err(Error::Conflict),
    }
}
