use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{patch, post, put},
};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    BranchProtection, app,
    auth::Principal,
    receive::{self, ReceiveError},
    repository_settings::{self, BranchProtections},
    server::Server,
};

const MAX_BRANCH_BYTES: usize = 255;

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/branches",
            post(create).delete(remove),
        )
        .route(
            "/api/repos/{owner}/{name}/settings/default-branch",
            patch(set_default),
        )
        .route(
            "/api/repos/{owner}/{name}/settings/branch-protections",
            put(set_branch_protections),
        )
        .route(
            "/api/repos/{owner}/{name}/settings/archive",
            put(set_archive),
        )
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    name: String,
    expected_oid: String,
}

#[derive(Serialize)]
struct DeleteOutput {
    branch: String,
    deleted_oid: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultBranchInput {
    name: String,
    expected_head: String,
    expected_oid: String,
}

#[derive(Serialize)]
struct DefaultBranchOutput {
    branch: String,
    commit: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchProtectionsInput {
    expected_version: u64,
    rules: Vec<BranchProtection>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveInput {
    expected_version: u64,
    archived: bool,
    repository: String,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Input(&'static str),
    #[error("Write access is required to manage branches")]
    Permission,
    #[error("Administrator access is required to change repository settings")]
    AdminPermission,
    #[error("Protected branches cannot be changed in the browser")]
    Protected,
    #[error("The default branch cannot be deleted")]
    DefaultBranch,
    #[error("The branch changed or was already deleted")]
    Changed,
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
            Self::App(
                app::Error::Conflict
                | app::Error::Storage(crab_storage::StorageError::StateConflict { .. }),
            ) => (
                StatusCode::CONFLICT,
                "settings_changed",
                "Repository settings changed; reload before saving",
            ),
            Self::Body(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Enter branch names and commit IDs",
            ),
            Self::Permission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage branches",
            ),
            Self::AdminPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Administrator access is required to change repository settings",
            ),
            Self::Protected => (
                StatusCode::FORBIDDEN,
                "protected_branch",
                "Protected branches cannot be changed in the browser",
            ),
            Self::DefaultBranch => (
                StatusCode::FORBIDDEN,
                "default_branch",
                "The default branch cannot be deleted",
            ),
            Self::Changed => (
                StatusCode::CONFLICT,
                "branch_changed",
                "The branch changed or was already deleted; reload before retrying",
            ),
            Self::Receive(error)
                if matches!(error.as_ref(), ReceiveError::DefaultBranchChanged) =>
            {
                (
                    StatusCode::CONFLICT,
                    "default_branch_changed",
                    "The default branch changed; reload before retrying",
                )
            }
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::BranchChanged) => (
                StatusCode::CONFLICT,
                "branch_changed",
                "The selected branch changed or was deleted; reload before retrying",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Forbidden) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to create branches",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Protected) => (
                StatusCode::FORBIDDEN,
                "protected_branch",
                "Protected branches cannot be changed in the browser",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Archived) => (
                StatusCode::FORBIDDEN,
                "repository_archived",
                "This repository is archived and read-only",
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
    let branch = branch_ref(&input.name).map_err(Error::Input)?;
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

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<DeleteInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    let repository = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_write(&repository.config) {
        return Err(Error::Permission);
    }
    let branch = branch_ref(&input.name).map_err(Error::Input)?;
    let expected = ObjectId::from_hex(input.expected_oid.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::Input("Expected tip must be a full SHA-1 commit ID"))?;
    match receive::delete_branch(
        Arc::clone(&server),
        principal,
        (owner, name),
        branch.clone(),
        expected,
    )
    .await
    {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(DeleteOutput {
                branch,
                deleted_oid: expected.to_string(),
            }),
        )),
        Err(ReceiveError::Protected) => Err(Error::Protected),
        Err(ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Ref { .. })) => {
            Err(Error::DefaultBranch)
        }
        Err(
            ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Stale { .. })
            | ReceiveError::Write(crab_write::WriteError::RefChanged { .. }),
        ) => Err(Error::Changed),
        Err(error) => Err(error.into()),
    }
}

async fn set_default(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<DefaultBranchInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    let repository = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_admin(&repository.config) {
        return Err(Error::AdminPermission);
    }
    let branch = branch_ref(&input.name).map_err(Error::Input)?;
    let expected_head = input
        .expected_head
        .strip_prefix("refs/heads/")
        .and_then(|name| branch_ref(name).ok())
        .filter(|head| head == &input.expected_head)
        .ok_or(Error::Input("Expected HEAD must be a full branch ref"))?;
    let expected = ObjectId::from_hex(input.expected_oid.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::Input("Expected tip must be a full SHA-1 commit ID"))?;
    receive::set_default_branch(
        Arc::clone(&server),
        principal,
        (owner, name),
        expected_head,
        branch.clone(),
        expected,
    )
    .await?;
    Ok(Json(DefaultBranchOutput {
        branch,
        commit: expected.to_string(),
    }))
}

async fn set_branch_protections(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<BranchProtectionsInput>, JsonRejection>,
) -> Result<Json<BranchProtections>, Error> {
    let Json(input) = input?;
    let repository = app::repository(&server, &principal, &(owner, name))?;
    if !principal.can_admin(&repository.config) {
        return Err(Error::AdminPermission);
    }
    Ok(Json(
        repository_settings::replace(repository, input.expected_version, input.rules).await?,
    ))
}

async fn set_archive(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<ArchiveInput>, JsonRejection>,
) -> Result<Json<crate::repository_settings::RepositoryLifecycle>, Error> {
    let Json(input) = input?;
    let repository = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_admin(&repository.config) {
        return Err(Error::AdminPermission);
    }
    if input.repository != format!("{owner}/{name}") {
        return Err(Error::Input("Enter the full repository name to confirm"));
    }
    Ok(Json(
        repository_settings::replace_lifecycle(repository, input.expected_version, input.archived)
            .await?,
    ))
}

pub(crate) fn branch_ref(name: &str) -> Result<String, &'static str> {
    if name.is_empty()
        || name.len() > MAX_BRANCH_BYTES
        || name.trim() != name
        || name.starts_with("refs/")
        || name.chars().any(char::is_control)
    {
        return Err("Enter a valid branch name");
    }
    let branch = format!("refs/heads/{name}");
    crab_git::validate_push_refname(&branch).map_err(|_| "Enter a valid branch name")?;
    Ok(branch)
}
