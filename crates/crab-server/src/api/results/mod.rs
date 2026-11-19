use crate::{app, auth::Principal, repository::Repository, server::Server};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crab_workflow::{WorkflowError, WorkflowStore, results::*};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/results", get(read))
        .route(
            "/api/repos/{owner}/{name}/results/reports",
            post(create_report),
        )
        .layer(axum::extract::DefaultBodyLimit::max(96 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    request: String,
}
async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Input>,
) -> Response {
    let repo = match app::repository(&server, &principal, &key) {
        Ok(repo) => repo,
        Err(error) => return error.into_response(),
    };
    if input.request.len() > 32 * 1024 {
        return bad_request("Results query exceeds 32 KiB");
    }
    let request: ResultsRead = match serde_json::from_str(&input.request) {
        Ok(request) => request,
        Err(_) => return bad_request("Invalid Results request"),
    };
    let cancellation = server.cancellation.child_token();
    let _guard = cancellation.clone().drop_guard();
    let reader = match remote(&server, &repo, cancellation.clone()) {
        Ok(remote) => remote,
        Err(error) => return error.into_response(),
    };
    let outcome = ResultsReader::Remote(Box::new(reader)).read(request).await;
    // Revalidate current grants after bounded evidence reads, before returning bytes.
    if let Err(error) = app::membership(&server, &principal, &repo).await {
        return error.into_response();
    }
    match outcome {
        Ok(result) => ([("cache-control", "no-store")], Json(result)).into_response(),
        Err(error) => failure(error),
    }
}
async fn create_report(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Json(draft): Json<ReportDraft>,
) -> Response {
    let repo = match app::repository(&server, &principal, &key) {
        Ok(repo) => repo,
        Err(error) => return error.into_response(),
    };
    if !principal.can_write(repo.id) {
        return app::Error::Forbidden.into_response();
    }
    if draft.title.trim().is_empty() || draft.title.len() > 256 {
        return bad_request("Report title must contain 1–256 characters");
    }
    let cancellation = server.cancellation.child_token();
    let _guard = cancellation.clone().drop_guard();
    let reader = match remote(&server, &repo, cancellation.clone()) {
        Ok(remote) => remote,
        Err(error) => return error.into_response(),
    };
    let response = match ResultsReader::Remote(Box::new(reader))
        .read(ResultsRead::Compare {
            runs: draft.runs,
            spec: draft.spec,
        })
        .await
    {
        Ok(ResultsResponse::Comparison(view)) => view,
        Ok(_) => return bad_request("Expected a comparison"),
        Err(error) => return failure(error),
    };
    let report = match Report::new(draft.title, response.comparison, response.charts) {
        Ok(report) => report,
        Err(error) => return failure(error),
    };
    let current = match app::membership(&server, &principal, &repo).await {
        Ok(current) => current,
        Err(error) => return error.into_response(),
    };
    if !current.principal.can_write(repo.id) {
        return app::Error::Forbidden.into_response();
    }
    let reader = match remote(&server, &repo, cancellation) {
        Ok(remote) => remote,
        Err(error) => return error.into_response(),
    };
    let (send, receive) = tokio::sync::oneshot::channel();
    // The server drains publication and lease release even if HTTP times out.
    // The request drop guard cancels work; it never detaches a live writer.
    server.receives.spawn(async move {
        let _ = send.send(publish_report(&reader, &report).await);
    });
    match receive.await {
        Ok(Ok(id)) => Json(json!({"id":id})).into_response(),
        Ok(Err(error)) => failure(error),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"message":"Report worker stopped; refresh reports before retrying"}}))).into_response(),
    }
}
fn remote(
    server: &Server,
    repo: &Repository,
    cancellation: CancellationToken,
) -> Result<RemoteResults, super::ApiError> {
    let admission = server
        .runtime
        .read_admission(server.options.operation_limits(), cancellation.clone())?;
    let store = repo.store.clone().with_read_admission(admission);
    Ok(RemoteResults::new(
        WorkflowStore::from_storage(store),
        repo.layout.repo_prefix(),
        cancellation,
    ))
}
async fn publish_report(remote: &RemoteResults, report: &Report) -> crab_workflow::Result<String> {
    let directory = tempfile::tempdir()?;
    let local = LocalResults::new(directory.path())?;
    let mut total = 0_u64;
    let mut retained = std::collections::BTreeSet::new();
    // Use the storage owner's closure: linked inputs/models/checkpoints must
    // neither consume the report budget nor require their payloads to exist.
    let evidence = report
        .comparison
        .rows
        .iter()
        .flat_map(|row| &row.run.snapshot.attachments)
        .filter(|attachment| {
            attachment.state == Availability::Present
                && attachment.report_content(report.comparison.spec.include_logs)
        });
    for attachment in evidence {
        if let Some(hash) = &attachment.hash
            && retained.insert(hash)
        {
            total += attachment.size;
            if total > MAX_CAPTURE_BYTES as u64 {
                return Err(WorkflowError::Configuration {
                    key: "results".into(),
                    origin: "Report evidence exceeds 256 MiB".into(),
                });
            }
            local.put_blob(&remote.blob(hash).await?)?;
        }
    }
    local.save_report(report)?;
    remote.push_report(&local, report).await
}
fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"message":message}})),
    )
        .into_response()
}
fn failure(error: WorkflowError) -> Response {
    let (status, message) = match &error {
        WorkflowError::NotFound {..} => (StatusCode::NOT_FOUND, "Captured result or evidence was not found".to_owned()),
        WorkflowError::Configuration {origin, ..} => (StatusCode::BAD_REQUEST, origin.clone()),
        WorkflowError::Storage(object_store::Error::PermissionDenied {..} | object_store::Error::Unauthenticated {..}) =>
            (StatusCode::FORBIDDEN, "Storage access to retained evidence was denied".to_owned()),
        WorkflowError::StorageDomain(crab_storage::StorageError::Forbidden {..} | crab_storage::StorageError::NoCredentials | crab_storage::StorageError::AuthFailed {..} | crab_storage::StorageError::AuthExpired {..}) =>
            (StatusCode::FORBIDDEN, "Storage access to retained evidence was denied".to_owned()),
        WorkflowError::Cancelled => (StatusCode::REQUEST_TIMEOUT, "Results read was cancelled".to_owned()),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "Results evidence could not be verified or retrieved; retry or check repository storage".to_owned()),
    };
    (status, Json(json!({"error":{"message":message}}))).into_response()
}

#[cfg(test)]
mod tests;
