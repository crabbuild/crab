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
    server::{Repository, Server},
};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not found")]
    NotFound,
    #[error("Only the author can edit this content")]
    Forbidden,
    #[error("This content changed; reload before saving your draft")]
    Conflict,
    #[error(
        "This submission ID was already used for different content; check the existing discussion before submitting again"
    )]
    RequestConflict,
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

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Discussion not found"),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Only the author can edit this content",
            ),
            Self::Conflict | Self::Storage(StorageError::StateConflict { .. }) => (
                StatusCode::CONFLICT,
                "conflict",
                "This content changed; reload before saving your draft",
            ),
            Self::RequestConflict => (
                StatusCode::CONFLICT,
                "submission_conflict",
                "This submission ID was already used for different content; check the existing discussion before submitting again",
            ),
            Self::Body(error) => (
                error.status(),
                "invalid_request",
                "Invalid JSON request or request body too large",
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

pub(crate) fn repository<'a>(
    server: &'a Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<&'a Repository> {
    server
        .repositories
        .get(key)
        .filter(|repo| principal.can_read(&repo.config))
        .ok_or(Error::NotFound)
}

pub(crate) fn actor(principal: &Principal) -> Result<Identity> {
    let mut identity = principal.identity().ok_or(Error::Forbidden)?;
    identity.name = identity.name.chars().take(160).collect();
    Ok(identity)
}

pub(crate) fn number(value: u64) -> Result<u64> {
    if value == 0 || value >= crate::app_storage::MAX_NUMBER {
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
    Ok(value.to_ascii_lowercase())
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

pub(crate) fn body(value: &str, required: bool) -> Result<()> {
    if value.len() > 65_536 || value.contains('\0') || (required && value.trim().is_empty()) {
        return Err(Error::Invalid(
            "Content must be at most 64 KiB, without NUL characters; comments cannot be empty",
        ));
    }
    Ok(())
}
