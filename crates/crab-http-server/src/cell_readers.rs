//! Operator-owned target count for repository Cell read replicas.

use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use crab_cell_runtime::{Error as CellError, read_policy::MAX_READERS};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{app, auth::Principal, server::Server};

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/settings/read-replicas",
            get(read).put(update),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    expected_revision: u64,
    desired_readers: u16,
}

#[derive(Serialize)]
struct TargetOutput {
    desired_readers: u16,
    revision: u64,
    stale_incarnation: bool,
    convergence: &'static str,
}

enum Error {
    NotFound,
    Forbidden,
    Unavailable,
    Conflict,
    Invalid,
    Body,
    Cell(CellError),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::NotFound | Self::Cell(CellError::CellNotActive) => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Repository Cell not found",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Administrator access is required",
            ),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Read replicas require the object durability profile",
            ),
            Self::Conflict
            | Self::Cell(CellError::Storage(crab_storage::StorageError::StateConflict {
                ..
            })) => (
                StatusCode::CONFLICT,
                "policy_changed",
                "Read-replica target changed; reload before retrying",
            ),
            Self::Invalid | Self::Body => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid read-replica target request",
            ),
            Self::Cell(error) => {
                tracing::error!(error = %error, "read-replica target request failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "Read-replica target could not be read or updated",
                )
            }
        };
        (status, Json(json!({"code": code, "message": message}))).into_response()
    }
}

fn resolve(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<
    (
        crate::cells::ReadReplicaManager,
        crab_cell_runtime::CellTarget,
    ),
    Error,
> {
    let repository = app::repository(server, principal, key).map_err(|_| Error::NotFound)?;
    if !principal.can_admin(&repository.config) {
        return Err(Error::Forbidden);
    }
    let router = server.repository_cells().ok_or(Error::Unavailable)?;
    let manager = server
        .peer_receiver()
        .and_then(|receiver| receiver.read_replicas())
        .ok_or(Error::Unavailable)?;
    let target = router
        .repository_target(repository.id)
        .map_err(Error::Cell)?;
    Ok((manager, target))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<TargetOutput>, Error> {
    let (manager, target) = resolve(&server, &principal, &key)?;
    let (incarnation, policy) = manager.target(&target).await.map_err(Error::Cell)?;
    let stale = policy.is_some_and(|policy| policy.incarnation() != incarnation);
    let current = policy.filter(|_| !stale);
    Ok(Json(TargetOutput {
        desired_readers: current.map_or(0, |policy| policy.desired_readers()),
        revision: policy.map_or(0, |policy| policy.revision()),
        stale_incarnation: stale,
        convergence: "unverified",
    }))
}

async fn update(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<UpdateInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input.map_err(|_| Error::Body)?;
    if input.desired_readers > MAX_READERS {
        return Err(Error::Invalid);
    }
    let (manager, target) = resolve(&server, &principal, &key)?;
    let policy = manager
        .set_target(&target, input.expected_revision, input.desired_readers)
        .await
        .map_err(Error::Cell)?
        .ok_or(Error::Conflict)?;
    if let Some(router) = server.repository_cells()
        && let Err(error) = router.hint_read_replica_target(target).await
    {
        // The S3 policy CAS has succeeded; the bounded owner loop retries this hint.
        tracing::warn!(error = %error, "read replica target hint failed");
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(TargetOutput {
            desired_readers: policy.desired_readers(),
            revision: policy.revision(),
            stale_incarnation: false,
            convergence: "pending",
        }),
    ))
}
