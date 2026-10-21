use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use crab_storage::StorageError;
use serde_json::json;

use crate::{
    auth::{Identity, Principal},
    repository::Repository,
    server::Server,
};

pub(crate) const MAX_NUMBER: u64 = 9_007_199_254_740_991;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not found")]
    NotFound,
    #[error("Commit not found")]
    CommitNotFound,
    #[error("Only the author can edit this content")]
    Forbidden,
    #[error("Archived repositories are read-only")]
    Archived,
    #[error("Pull request authors cannot approve or request changes on their own changes")]
    OwnReview,
    #[error("Draft pull requests must be marked ready before merging")]
    DraftPull,
    #[error("This content changed; reload before saving your draft")]
    Conflict,
    #[error("The pull request changes conflict or its branch tips changed")]
    MergeConflict,
    #[error("Git writes are busy")]
    MergeBusy,
    #[error("Write access is required to merge pull requests")]
    MergePermission,
    #[error("A pull request merge is in progress")]
    MergePending,
    #[error("Protected branch review requirements are not satisfied")]
    MergeBlocked,
    #[error("Write access is required to report commit statuses")]
    StatusPermission,
    #[error("Write access is required to manage check runs")]
    CheckPermission,
    #[error("Check run not found")]
    CheckNotFound,
    #[error("Write access is required to manage repository labels")]
    LabelPermission,
    #[error("Write access is required to manage milestones")]
    MilestonePermission,
    #[error("Write access is required to manage linked work")]
    LinkPermission,
    #[error("A milestone with this title already exists")]
    MilestoneConflict,
    #[error("A repository label with this name already exists")]
    LabelConflict,
    #[error("Repository label not found")]
    LabelNotFound,
    #[error("Write access is required to manage assignees")]
    AssigneePermission,
    #[error("Write access is required to manage releases")]
    ReleasePermission,
    #[error("A release already exists for this tag or the tag points to another commit")]
    ReleaseConflict,
    #[error("Git writes are busy")]
    ReleaseBusy,
    #[error("Release staging failed")]
    ReleaseStaging(#[source] crate::local_disk::Error),
    #[error("Release not found")]
    ReleaseNotFound,
    #[error("Release asset not found")]
    ReleaseAssetNotFound,
    #[error("A release asset with this name already exists")]
    ReleaseAssetConflict,
    #[error("Release asset is too large")]
    ReleaseAssetTooLarge,
    #[error("Release asset upload was cancelled or timed out")]
    ReleaseAssetCancelled,
    #[error("Invalid release asset body")]
    ReleaseAssetBody(#[source] axum::Error),
    #[error("Release asset temporary file operation failed")]
    ReleaseAssetIo(#[source] std::io::Error),
    #[error("Release asset transfer admission failed")]
    ReleaseAssetCoordination(#[source] crab_coordination::CoordinationError),
    #[error("Release tag publication failed")]
    Release(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("Pull request merge failed")]
    Merge(#[source] Box<crate::git::receive::ReceiveError>),
    #[error("Pull request merge object construction failed")]
    MergeObject(#[source] Box<crab_remote::objects::Error>),
    #[error(
        "This submission ID was already used for different content; check the existing discussion before submitting again"
    )]
    RequestConflict,
    #[error("Collaboration Cell routing is unavailable")]
    CellUnavailable,
    #[error("Collaboration Cell contract failed: {0}")]
    CellContract(&'static str),
    #[error("Collaboration Cell operation failed")]
    Cell(#[source] crate::cells::Error),
    #[error("Collaboration storage failed")]
    Storage(#[from] StorageError),
    #[error("Collaboration data encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("Invalid request body")]
    Body(#[from] JsonRejection),
    #[error("Clock failed")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("Repository read failed")]
    Repository(#[from] crate::Error),
}
pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn mutation_identity() -> Result<crate::cells::MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Cell)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(crate::cells::MutationIdentity {
        request_id: uuid::Uuid::now_v7(),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

impl From<&Identity> for crate::cells::repository::RepositoryAuthor {
    fn from(identity: &Identity) -> Self {
        Self {
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
            name: identity.name.clone(),
        }
    }
}

pub(crate) fn repository_cell(
    server: &Server,
    repository: &Repository,
) -> Result<crate::cells::RepositoryCell> {
    server
        .repository_cells()
        .ok_or(Error::CellUnavailable)?
        .repository(repository.id)
        .map_err(Error::Cell)
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let expected_lock_conflict = matches!(
            &self,
            Self::Merge(error) if matches!(error.as_ref(), crate::git::receive::ReceiveError::Locked)
        );
        if !expected_lock_conflict
            && matches!(
                self,
                Self::Storage(_)
                    | Self::Json(_)
                    | Self::Clock(_)
                    | Self::Repository(_)
                    | Self::Merge(_)
                    | Self::MergeObject(_)
                    | Self::Release(_)
                    | Self::ReleaseAssetIo(_)
                    | Self::ReleaseAssetCoordination(_)
                    | Self::ReleaseStaging(_)
                    | Self::CellContract(_)
                    | Self::Cell(_)
            )
        {
            tracing::error!(error = ?self, "collaboration request failed");
        }
        let staging = match &self {
            Self::Merge(error) | Self::Release(error) => error.staging_status(),
            _ => None,
        };
        if let Some((status, message)) = staging {
            return (
                status,
                Json(json!({"error":{"code":"staging","message":message}})),
            )
                .into_response();
        }
        let (status, code, message) = match &self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Discussion not found"),
            Self::CommitNotFound => (StatusCode::NOT_FOUND, "not_found", "Commit not found"),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Only the author can edit this content",
            ),
            Self::Archived => (
                StatusCode::FORBIDDEN,
                "repository_archived",
                "This repository is archived and read-only",
            ),
            Self::OwnReview => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Pull request authors cannot approve or request changes on their own changes",
            ),
            Self::Conflict | Self::Storage(StorageError::StateConflict { .. }) => (
                StatusCode::CONFLICT,
                "conflict",
                "This content changed; reload before saving your draft",
            ),
            Self::MergeConflict => (
                StatusCode::CONFLICT,
                "merge_conflict",
                "The branch tips changed or these changes conflict. Reload and review the latest commits",
            ),
            Self::MergeBusy => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy; retry this merge shortly",
            ),
            Self::MergePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to merge pull requests",
            ),
            Self::MergePending => (
                StatusCode::CONFLICT,
                "merge_pending",
                "A merge is in progress. Reload the pull request and retry that merge before editing",
            ),
            Self::DraftPull => (
                StatusCode::CONFLICT,
                "pull_draft",
                "Draft pull requests must be marked ready before merging",
            ),
            Self::MergeBlocked => (
                StatusCode::CONFLICT,
                "merge_blocked",
                "Required approvals or status checks are not satisfied on the current head",
            ),
            Self::StatusPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to report commit statuses",
            ),
            Self::CheckPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage check runs",
            ),
            Self::CheckNotFound => (StatusCode::NOT_FOUND, "not_found", "Check run not found"),
            Self::MilestonePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage milestones",
            ),
            Self::LinkPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage linked work",
            ),
            Self::MilestoneConflict => (
                StatusCode::CONFLICT,
                "milestone_conflict",
                "A milestone with this title already exists",
            ),
            Self::LabelPermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage repository labels",
            ),
            Self::LabelConflict => (
                StatusCode::CONFLICT,
                "label_conflict",
                "A repository label with this name already exists",
            ),
            Self::LabelNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Repository label not found",
            ),
            Self::AssigneePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage assignees",
            ),
            Self::ReleasePermission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to manage releases",
            ),
            Self::ReleaseConflict => (
                StatusCode::CONFLICT,
                "release_conflict",
                "A release already exists for this tag or the tag points to another commit",
            ),
            Self::ReleaseStaging(crate::local_disk::Error::TooLarge) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "asset_too_large",
                "Release staging exceeds the node disk capacity",
            ),
            Self::ReleaseBusy | Self::ReleaseStaging(crate::local_disk::Error::Busy(_)) => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy; retry this release shortly",
            ),
            Self::ReleaseNotFound => (StatusCode::NOT_FOUND, "not_found", "Release not found"),
            Self::ReleaseAssetNotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Release asset not found",
            ),
            Self::ReleaseAssetConflict => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "asset_conflict",
                "A release asset with this name already exists",
            ),
            Self::ReleaseAssetTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "asset_too_large",
                "Release assets must be no larger than 512 MiB",
            ),
            Self::ReleaseAssetCancelled
            | Self::ReleaseStaging(crate::local_disk::Error::Cancelled) => (
                StatusCode::REQUEST_TIMEOUT,
                "asset_cancelled",
                "Release asset upload was cancelled or timed out",
            ),
            Self::ReleaseAssetBody(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid release asset body",
            ),
            Self::ReleaseAssetIo(_)
            | Self::ReleaseStaging(_)
            | Self::ReleaseAssetCoordination(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "asset_storage",
                "Release asset storage is unavailable. Retry shortly",
            ),
            Self::RequestConflict => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content; check the existing discussion before submitting again",
            ),
            Self::Cell(crate::cells::Error::StaleRead) => (
                StatusCode::CONFLICT,
                "stale_read",
                "Repository Cell has not reached the requested revision",
            ),
            Self::Cell(crate::cells::Error::Pending { .. }) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "outcome_unknown",
                "The write may have completed; retry the same submission to resolve it",
            ),
            Self::CellUnavailable | Self::Cell(crate::cells::Error::Unavailable) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "cell_unavailable",
                "Repository collaboration is temporarily unavailable; retry the same submission shortly",
            ),
            Self::Cell(crate::cells::Error::RequestConflict) => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content",
            ),
            Self::Cell(crate::cells::Error::Expired) => (
                StatusCode::GONE,
                "request_expired",
                "Repository request expired before it could execute",
            ),
            Self::CellContract(_) | Self::Cell(_) => (
                StatusCode::BAD_GATEWAY,
                "cell_error",
                "Repository collaboration runtime failed; retry the same submission after checking service health",
            ),
            Self::Body(error) => (
                error.status(),
                "invalid_request",
                "Invalid JSON request or request body too large",
            ),
            Self::Merge(error)
                if matches!(error.as_ref(), crate::git::receive::ReceiveError::Locked) =>
            {
                (
                    StatusCode::CONFLICT,
                    "path_locked",
                    "A changed path is locked by another user",
                )
            }
            Self::Merge(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "merge_failed",
                "The merge may have completed. Reload the pull request before retrying the same submission",
            ),
            Self::Release(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "release_failed",
                "The release tag may have been published. Retry the same submission to recover it",
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "storage_error",
                "Collaboration storage is unavailable. A write may have succeeded; retry the same submission to recover it",
            ),
        };

        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

pub(crate) async fn admit(
    State(server): State<Arc<Server>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = server.app_admission.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":{"message":"Collaboration requests are busy; retry shortly"}})),
        )
            .into_response();
    };
    // Admission covers producing the response, not consuming its body. Streaming
    // handlers must retain their own transfer permit and cancellation ownership.
    let started = Instant::now();
    let response = tokio::select! {
        () = server.cancellation.cancelled() => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"Server is shutting down; retry the same submission after restart"}}))).into_response(),
        response = tokio::time::timeout(Duration::from_secs(30), next.run(request)) => match response {
            Ok(response) => response,
            Err(_) => (StatusCode::GATEWAY_TIMEOUT, Json(json!({"error":{"message":"Collaboration request timed out; retry the same submission to recover a possible completed write"}}))).into_response(),
        },
    };
    (
        [(
            "server-timing",
            format!("app;dur={:.3}", started.elapsed().as_secs_f64() * 1000.0),
        )],
        response,
    )
        .into_response()
}

pub(crate) fn repository(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<Arc<Repository>> {
    server
        .repositories
        .get(key)
        .filter(|repo| principal.can_read(&repo.config))
        .ok_or(Error::NotFound)
}

pub(crate) struct Membership {
    pub members: Vec<crate::RepositoryMember>,
    pub can_write: bool,
}

pub(crate) async fn membership(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
) -> Result<Membership> {
    if matches!(principal, Principal::Local) {
        return Ok(Membership {
            members: Vec::new(),
            can_write: true,
        });
    }
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    // Participant selection must use durable membership, not a background
    // materialization that could still offer a renamed or revoked reviewer.
    let snapshot = catalog
        .membership(&repo.config.owner, &repo.config.name)
        .await
        .map_err(|error| match error {
            crate::catalog::CatalogError::NotFound => Error::NotFound,
            error => Error::Repository(crate::Error::Catalog(error)),
        })?;
    let mut config = repo.config.clone();
    config.members = snapshot.members;
    if !principal.can_read(&config) {
        return Err(Error::NotFound);
    }
    Ok(Membership {
        can_write: principal.can_write(&config),
        members: config.members,
    })
}

pub(crate) fn actor(principal: &Principal) -> Result<Identity> {
    let mut identity = principal.identity().ok_or(Error::Forbidden)?;
    identity.name = identity.name.chars().take(160).collect();
    Ok(identity)
}

pub(crate) fn number(value: u64) -> Result<u64> {
    if value == 0 || value >= MAX_NUMBER {
        return Err(Error::NotFound);
    }
    Ok(value)
}

pub(crate) fn submission(value: &str) -> Result<String> {
    if value.len() != 36
        || !value.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(Error::Invalid("Submission ID must be a UUID"));
    }
    let value = value.to_ascii_lowercase();
    tracing::debug!(
        target: "crab_server::action",
        event = "application_submission",
        submission_id = %value,
    );
    Ok(value)
}

pub(crate) fn submission_id(value: &str) -> Result<[u8; 16]> {
    uuid::Uuid::parse_str(&submission(value)?)
        .map(uuid::Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

pub(crate) fn title(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Title must contain 1–256 characters without control characters",
        ));
    }
    Ok(value.to_owned())
}

pub(crate) fn search_query(value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.chars().count() > 256 || value.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Search query must be at most 256 characters without controls",
        ));
    }
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    crate::cells::repository::search::Query::parse(Some(value)).map_err(Error::Invalid)?;
    Ok(Some(value.to_owned()))
}

pub(crate) fn body(value: &str, required: bool) -> Result<()> {
    if value.len() > 65_536 || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(Error::Invalid(
            "Content must be at most 64 KiB, without NUL characters; comments cannot be empty",
        ));
    }
    Ok(())
}

pub(crate) fn command_output<T>(
    result: crate::cells::Result<crate::cells::Committed<T>>,
) -> Result<T> {
    result
        .map(|committed| committed.output)
        .map_err(Error::Cell)
}

pub(crate) fn query_output<T>(
    result: crate::cells::Result<crate::cells::Observed<T>>,
) -> Result<T> {
    result.map(|observed| observed.output).map_err(Error::Cell)
}
