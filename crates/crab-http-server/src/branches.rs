use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    app,
    auth::Principal,
    receive::{self, ReceiveError},
    server::Server,
};

const MAX_BRANCH_BYTES: usize = 255;

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/branches",
        post(create).layer(axum::extract::DefaultBodyLimit::max(2048)),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    name: String,
    source_oid: String,
}

#[derive(Serialize)]
struct CreateOutput {
    branch: String,
    commit: String,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Input(&'static str),
    #[error("Write access is required to create branches")]
    Permission,
    #[error("Repository request failed")]
    App(#[from] app::Error),
    #[error("Repository publication failed")]
    Receive(#[source] Box<ReceiveError>),
    #[error("Invalid request body")]
    Body(#[from] JsonRejection),
}

impl From<ReceiveError> for Error {
    fn from(error: ReceiveError) -> Self {
        Self::Receive(Box::new(error))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Input(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::App(app::Error::NotFound) => (
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found",
            ),
            Self::App(app::Error::Invalid(message)) => {
                (StatusCode::BAD_REQUEST, "invalid_request", *message)
            }
            Self::Body(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Enter a branch name and source commit",
            ),
            Self::Permission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to create branches",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Forbidden) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to create branches",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Protected) => (
                StatusCode::FORBIDDEN,
                "protected_branch",
                "This protected branch cannot be created in the browser",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Busy) => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy; retry shortly",
            ),
            Self::Receive(error)
                if matches!(
                    error.as_ref(),
                    ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
                        | ReceiveError::Write(crab_write::WriteError::RefChanged { .. })
                        | ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Namespace(
                            _
                        ))
                        | ReceiveError::Write(crab_write::WriteError::Namespace(_))
                ) =>
            {
                (
                    StatusCode::CONFLICT,
                    "branch_exists",
                    "A branch with this name already exists or conflicts with another branch",
                )
            }
            Self::Receive(error)
                if matches!(
                    error.as_ref(),
                    ReceiveError::Graph(
                        crab_git::receive_plan::ReceivePlanError::Missing { .. }
                            | crab_git::receive_plan::ReceivePlanError::Kind { .. }
                            | crab_git::receive_plan::ReceivePlanError::Invalid { .. }
                            | crab_git::receive_plan::ReceivePlanError::Parse { .. }
                    )
                ) =>
            {
                (
                    StatusCode::CONFLICT,
                    "invalid_source",
                    "The source commit is no longer available; reload before retrying",
                )
            }
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Request(_)) => (
                StatusCode::CONFLICT,
                "conflict",
                "The repository changed; reload before retrying",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_failed",
                "The branch could not be created. Reload the repository before retrying",
            ),
        };
        if status.is_server_error() {
            tracing::error!(error = ?self, "browser branch publication failed");
        }
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<CreateInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    let repository = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_write(&repository.config) {
        return Err(Error::Permission);
    }
    let branch = branch_ref(&input.name)?;
    let source = ObjectId::from_hex(input.source_oid.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::Input("Source must be a full SHA-1 commit ID"))?;
    receive::create_branch(
        Arc::clone(&server),
        principal,
        (owner, name),
        branch.clone(),
        source,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreateOutput {
            branch,
            commit: source.to_string(),
        }),
    ))
}

fn branch_ref(name: &str) -> Result<String, Error> {
    if name.is_empty()
        || name.len() > MAX_BRANCH_BYTES
        || name.trim() != name
        || name.starts_with("refs/")
        || name.chars().any(char::is_control)
    {
        return Err(Error::Input("Enter a valid branch name"));
    }
    let branch = format!("refs/heads/{name}");
    crab_git::validate_push_refname(&branch)
        .map_err(|_| Error::Input("Enter a valid branch name"))?;
    Ok(branch)
}
