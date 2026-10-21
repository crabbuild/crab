mod activity;
mod archive;
mod assignees;
mod branches;
mod catalog;
mod checks;
mod contents;
mod issues;
mod labels;
mod members;
mod milestones;
mod pulls;
mod reactions;
mod releases;
mod statuses;

mod query;
mod representation;
pub(crate) use query::comparison_blobs;
use query::execute;
use representation::{asset_response, blob_response, decode_cursor, decode_hex};
pub(crate) use representation::{display_path, encode_hex};

use std::sync::Arc;
use std::time::Instant;

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_remote_git::{Blob, CorruptionStage, Error, GitPath, PageRequest, RevisionError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::Principal, repository::MaintenanceTrigger, repository::projection, server::Server,
};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Action {
    Refs,
    Commit,
    Commits,
    Tree,
    #[serde(rename = "tree-attribution")]
    TreeAttribution,
    Search,
    Blob,
    Asset,
    File,
    Changes,
    Diff,
    Blame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttributionSource<'a> {
    Projected(&'a str),
    Canonical,
    Indexing,
}

fn attribution_source<'a>(
    source: &'a crate::cells::repository::projection::SourceIdentity,
    state: &crate::cells::repository::projection::ProjectionStateView,
    path_state_available: bool,
) -> AttributionSource<'a> {
    if state.source_token.as_deref() == Some(source.source_token.as_str()) {
        AttributionSource::Projected(&source.source_token)
    } else if source.path_state_hash.is_some() && path_state_available {
        AttributionSource::Canonical
    } else {
        AttributionSource::Indexing
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Parameters {
    rev: Option<String>,
    base: Option<String>,
    path: Option<String>,
    path_hex: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
    q: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum ApiError {
    #[error("{0}")]
    Input(&'static str),
    #[error("remote Git operation failed")]
    Remote(#[from] Error),
    #[error("repository service failed")]
    Service(#[from] crate::Error),
    #[error("JSON response encoding failed")]
    Json(#[from] serde_json::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::Service(crate::Error::Maintenance(error)) = &self {
            tracing::error!(error = ?error, "repository maintenance request failed");
        }
        if let Self::Service(crate::Error::Worker(error)) = &self {
            tracing::error!(error = %error, "repository maintenance task failed");
        }
        let (status, code, message) = match &self {
            Self::Input(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::Remote(error) | Self::Service(crate::Error::Remote(error)) => match error {
                Error::EmptyRepository => (
                    StatusCode::NOT_FOUND,
                    "empty_repository",
                    "This repository has no commits yet",
                ),
                Error::PathNotFound
                | Error::SnapshotUnavailable
                | Error::Revision {
                    reason: RevisionError::NotFound | RevisionError::NotReachable,
                } => (
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "The requested path or revision is not available",
                ),
                Error::InvalidCursor { .. } => (
                    StatusCode::CONFLICT,
                    "stale_cursor",
                    "This page has changed. Reload the first page to continue",
                ),
                Error::InvalidPath { .. }
                | Error::InvalidLimit { .. }
                | Error::Revision { .. }
                | Error::EntryNotBlob { .. }
                | Error::PathComponentNotTree { .. } => (
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Invalid path, revision, or entry kind",
                ),
                Error::BlameUnsupported { .. } => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "blame_unsupported",
                    "Blame requires an ordinary UTF-8 text file",
                ),
                Error::LimitExceeded { .. } => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "read_limit",
                    "This request exceeds the repository read budget",
                ),
                Error::Timeout { .. } => (
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "The repository read timed out. Try a smaller request",
                ),
                Error::Cancelled => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cancelled",
                    "The read was cancelled",
                ),
                Error::RepositoryIndexing { .. } | Error::PathStateIndexing { .. } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "indexing",
                    "Repository metadata is still being indexed. Retry shortly",
                ),
                Error::Corrupt {
                    stage: CorruptionStage::PathState,
                } => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "path_state_corrupt",
                    "Repository attribution metadata is being rebuilt. Retry shortly",
                ),
                _ => (
                    StatusCode::BAD_GATEWAY,
                    "remote_read",
                    "Repository data could not be read. Check storage and repository health",
                ),
            },
            Self::Service(crate::Error::BrowseIndexes(_)) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "path_state_corrupt",
                "Repository attribution metadata is being rebuilt. Retry shortly",
            ),
            Self::Service(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "indexing_failed",
                "Repository indexing could not finish. Check storage permissions and server logs, then retry",
            ),
            Self::Json(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "encoding",
                "The response could not be encoded",
            ),
        };
        (
            status,
            Json(json!({"error": {"code": code, "message": message}})),
        )
            .into_response()
    }
}

enum Payload {
    Json(Value),
    Blob(Blob),
    Asset(Blob),
}

pub(crate) async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, action)): Path<(String, String, Action)>,
    Query(params): Query<Parameters>,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let Some(entry) = server
        .repositories
        .get(&(owner, name))
        .filter(|entry| principal.can_read(&entry.config))
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(
                json!({"error": {"code":"repository_not_found", "message":"Repository not found"}}),
            ),
        )
            .into_response();
    };
    let Ok(_permit) = server.admission.try_acquire() else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "1")],
            Json(json!({"error": {"code":"busy", "message":"The server is busy. Retry shortly"}})),
        )
            .into_response();
    };
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let mut open_ms = 0.0;
    let mut generation = None;
    let result = async {
        let limit = params.limit.unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return Err(ApiError::Input("limit must be between 1 and 200"));
        }
        let path = match (&params.path, &params.path_hex) {
            (Some(_), Some(_)) => return Err(ApiError::Input("use path or path_hex, not both")),
            (_, Some(value)) => GitPath::new(decode_hex(value)?)?,
            (Some(value), _) => GitPath::new(value.as_bytes().to_vec())?,
            _ => GitPath::root(),
        };
        if params.base.is_some() && !path.is_root() && matches!(action, Action::Commits) {
            return Err(ApiError::Input("base cannot be combined with path history"));
        }
        let cursor = params
            .cursor
            .as_deref()
            .map(|value| decode_cursor(&server.cursor_key, value))
            .transpose()?;
        let page = PageRequest::new(limit, cursor)?;
        let search_query = if matches!(action, Action::Search) {
            let query = params
                .q
                .as_deref()
                .map(str::trim)
                .filter(|query| !query.is_empty())
                .ok_or(ApiError::Input("q must contain a file name or path"))?;
            if query.chars().count() > 128 || query.chars().any(char::is_control) {
                return Err(ApiError::Input(
                    "q must contain at most 128 non-control characters",
                ));
            }
            Some(query)
        } else {
            None
        };
        let timer = Instant::now();
        let opened = entry.open(&server, &cancellation).await;
        open_ms = timer.elapsed().as_secs_f64() * 1000.0;
        let opened = opened?;
        let repository = &opened.git;
        generation = Some(repository.generation());
        let projection_source = if matches!(action, Action::TreeAttribution) {
            if let Some(router) = server.repository_cells() {
                // Attribution must identify the same immutable snapshot as the
                // directory reader, even if refs advance during this request.
                let source = &opened.source;
                projection::validate_source(&entry, source, &cancellation).await?;
                let state = projection::state(&router, entry.id).await?;
                let attribution_source =
                    attribution_source(source, &state, repository.path_state_available());
                let ready = matches!(attribution_source, AttributionSource::Projected(_));
                let lag_generations = state
                    .generation
                    .map_or(source.manifest_generation, |generation| {
                        source.manifest_generation.saturating_sub(generation)
                    });
                server
                    .metrics
                    .update_projection_state(ready, lag_generations, 0.0, 0);
                match attribution_source {
                    AttributionSource::Projected(source_token) => Some(source_token.to_owned()),
                    AttributionSource::Canonical => {
                        entry
                            .schedule_maintenance(&server, MaintenanceTrigger::Repair)
                            .await?;
                        None
                    }
                    AttributionSource::Indexing => {
                        entry
                            .schedule_maintenance(&server, MaintenanceTrigger::Repair)
                            .await?;
                        return Ok((
                            StatusCode::ACCEPTED,
                            [("retry-after", "2")],
                            Json(json!({"state":"indexing", "retry_after_ms": 2000})),
                        )
                            .into_response());
                    }
                }
            } else if !repository.path_state_available() {
                entry
                    .schedule_maintenance(&server, MaintenanceTrigger::Repair)
                    .await?;
                return Ok((
                    StatusCode::ACCEPTED,
                    [("retry-after", "2")],
                    Json(json!({"state":"indexing", "retry_after_ms": 2000})),
                )
                    .into_response());
            } else {
                None
            }
        } else {
            None
        };
        let payload = execute(
            &server,
            repository,
            entry.id,
            action,
            &params,
            path,
            page,
            search_query,
            &cancellation,
            projection_source.as_deref(),
        )
        .await?;
        match payload {
            Payload::Json(value) => {
                let bytes = serde_json::to_vec(&value)?;
                if bytes.len() > 8 * 1024 * 1024 {
                    return Err(ApiError::Remote(Error::LimitExceeded {
                        limit: "HTTP response bytes",
                        actual: bytes.len() as u64,
                        maximum: 8 * 1024 * 1024,
                    }));
                }
                Ok(([("content-type", "application/json")], bytes).into_response())
            }
            Payload::Blob(blob) => Ok(blob_response(blob, headers.get(header::RANGE))),
            Payload::Asset(blob) => Ok(asset_response(blob)),
        }
    }
    .await;
    if matches!(
        &result,
        Err(ApiError::Remote(Error::Corrupt {
            stage: CorruptionStage::PathState,
        })) | Err(ApiError::Service(crate::Error::Remote(Error::Corrupt {
            stage: CorruptionStage::PathState,
        }))) | Err(ApiError::Remote(Error::PathStateIndexing { .. }))
            | Err(ApiError::Service(crate::Error::BrowseIndexes(_)))
    ) && let Err(error) = entry
        .schedule_maintenance(&server, MaintenanceTrigger::Repair)
        .await
    {
        tracing::warn!(error = %error, "failed to schedule path-state repair");
    }
    let response = match result {
        Ok(value) => value,
        Err(error) => error.into_response(),
    };
    let total = started.elapsed().as_secs_f64() * 1000.0;
    (
        [
            (
                "server-timing",
                format!(
                    "open;dur={open_ms:.3}, read;dur={:.3}, total;dur={total:.3}",
                    total - open_ms
                ),
            ),
            (
                "x-crab-generation",
                generation
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
        ],
        response,
    )
        .into_response()
}

pub(crate) fn routes(server: Arc<Server>) -> axum::Router<Arc<Server>> {
    axum::Router::new()
        .merge(members::routes())
        .merge(milestones::routes(Arc::clone(&server)))
        .merge(activity::routes(Arc::clone(&server)))
        .merge(reactions::routes(Arc::clone(&server)))
        .merge(assignees::routes(Arc::clone(&server)))
        .merge(branches::routes())
        .merge(checks::routes(Arc::clone(&server)))
        .merge(contents::routes())
        .merge(issues::routes(Arc::clone(&server)))
        .merge(labels::routes(Arc::clone(&server)))
        .merge(releases::routes(Arc::clone(&server)))
        .merge(pulls::routes(Arc::clone(&server)))
        .merge(statuses::routes(Arc::clone(&server)))
        .route("/api/repos", axum::routing::get(catalog::list))
        .route(
            "/api/repos/{owner}/{name}/archive",
            axum::routing::get(archive::download),
        )
        .route(
            "/api/repos/{owner}/{name}/{action}",
            axum::routing::get(read),
        )
}

#[cfg(test)]
mod tests;
