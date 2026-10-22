//! Private management routes, health checks, and runtime metrics.

use super::{
    APP_ADMISSION_CAPACITY, GIT_ADMISSION_CAPACITY, MAINTENANCE_ADMISSION_CAPACITY,
    READ_ADMISSION_CAPACITY, Server,
};
use crate::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

mod assignments;
mod directory;
mod search;

pub(crate) fn router(server: Arc<Server>, peers: Router) -> Router {
    Router::new()
        .merge(search::routes())
        .merge(assignments::routes())
        .merge(directory::routes())
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/readyz", get(readiness))
        .route("/capacity", get(render_capacity))
        .route("/integrityz", get(integrity_status))
        .route("/metrics", get(render_metrics))
        .route(
            "/internal/repositories/{owner}/{name}/cell",
            get(cell_status),
        )
        .route("/internal/cells/v1/scale-down", post(perform_scale_down))
        .with_state(server)
        .fallback_service(peers)
}

async fn perform_scale_down(State(server): State<Arc<Server>>) -> Response {
    server
        .shutdown_mode
        .send_replace(cellar::sdk::ShutdownMode::Handoff);
    server.cancellation.cancel();
    (StatusCode::ACCEPTED, Json(json!({"status": "draining"}))).into_response()
}

async fn cell_status(
    State(server): State<Arc<Server>>,
    axum::extract::Path(key): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(repository) = server.repositories.get(&key) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(router) = server.cells() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let result: Result<_> = async {
        let cell = router.repository(repository.id)?;
        let state = cell.client.query::<crate::cells::repository::GetRepositoryLifecycle>(&repository.id, None, ()).await?;
        Ok(json!({"runtime": "cellar", "version": "0.1.0", "repository": repository.id, "revision": state.receipt.revision, "lifecycle": state.output}))
    }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "repository Cell status failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn render_capacity(State(server): State<Arc<Server>>) -> Response {
    let capacity = server.cell_capacity.clone();
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(capacity),
    )
        .into_response()
}

async fn integrity_status(State(server): State<Arc<Server>>) -> Response {
    let mut failed = false;
    let mut complete = true;
    let repositories = server
        .repositories
        .values()
        .into_iter()
        .map(|repository| {
            let snapshot = repository.integrity.snapshot();
            failed |= snapshot.state == crate::integrity::State::Failed;
            complete &= snapshot.state == crate::integrity::State::Complete;
            json!({
                "owner": repository.config.owner,
                "name": repository.config.name,
                "proof": snapshot,
            })
        })
        .collect::<Vec<_>>();
    let (status, state) = if failed {
        (StatusCode::SERVICE_UNAVAILABLE, "failed")
    } else if complete {
        (StatusCode::OK, "complete")
    } else {
        (StatusCode::ACCEPTED, "pending")
    };
    (
        status,
        Json(json!({"status": state, "repositories": repositories})),
    )
        .into_response()
}

async fn render_metrics(State(server): State<Arc<Server>>) -> Response {
    let mut body = server.metrics.render(crate::metrics::RuntimeSnapshot {
        repositories: server.repositories.len(),
        catalog_healthy: server.catalog_healthy.load(Ordering::Acquire),
        draining: server.cancellation.is_cancelled(),
        receive_workers: server.receives.len(),
        admission_available: [
            server.admission.available_permits(),
            server.transfer_admission.available_permits(),
            server.app_admission.available_permits(),
            server.maintenance_admission.available_permits(),
        ],
        admission_capacity: [
            READ_ADMISSION_CAPACITY,
            GIT_ADMISSION_CAPACITY,
            APP_ADMISSION_CAPACITY,
            MAINTENANCE_ADMISSION_CAPACITY,
        ],
    });
    if let Some(services) = &server.cell_services {
        let work = services.application_work();
        let resources = services.sample_metrics();
        body.push_str(&services.event_metrics().prometheus());
        body.push_str(&format!("# TYPE crab_cellar_pending_events gauge\ncrab_cellar_pending_events {}\n# TYPE crab_cellar_retired_events_total counter\ncrab_cellar_retired_events_total {}\n# TYPE crab_cellar_rss_bytes gauge\ncrab_cellar_rss_bytes {}\n# TYPE crab_cellar_open_file_descriptors gauge\ncrab_cellar_open_file_descriptors {}\n", work.pending_events, work.retired_events, resources.rss_bytes, resources.open_fds));
    }
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn readiness(State(server): State<Arc<Server>>) -> Response {
    match tokio::time::timeout(Duration::from_secs(10), check_readiness(&server)).await {
        Ok(Ok(())) => Json(json!({"status":"ready"})).into_response(),
        Ok(Err(error)) => {
            tracing::warn!(error = ?error, "repository readiness check failed");
            readiness_unavailable()
        }
        Err(_) => {
            tracing::warn!("repository readiness check timed out");
            readiness_unavailable()
        }
    }
}

async fn check_readiness(server: &Server) -> Result<()> {
    if server.cancellation.is_cancelled() {
        return Err(crate::Error::Config("server is draining"));
    }
    if !server.accepts_new_cells() {
        return Err(crate::Error::Config("Cellar is not accepting requests"));
    }
    if !server.catalog_healthy.load(Ordering::Acquire) {
        return Err(crate::Error::Config("catalog refresh is unhealthy"));
    }
    let catalog = server
        .catalog()
        .ok_or(crate::Error::Config("catalog is unavailable"))?;
    catalog.load().await?;
    for repository in server.repositories.values() {
        let cell = server
            .cells()
            .ok_or(crate::Error::Config("repository Cells are unavailable"))?
            .repository(repository.id)?;
        let assignments = cell
            .client
            .query::<crate::cells::repository::assignments::GetAssignmentBatch>(
                &cell.target,
                None,
                None,
            )
            .await?;
        if assignments.output.state != crate::cells::repository::assignments::AssignmentState::Ready
        {
            return Err(crate::Error::Config(
                "repository assignments require explicit preparation",
            ));
        }
        let index = cell
            .client
            .query::<crate::cells::repository::search::index::GetSearchIndexStatus>(
                &cell.target,
                None,
                (),
            )
            .await?;
        if !matches!(index.output, crate::SearchIndexStatus::Ready { .. }) {
            return Err(crate::Error::Config(
                "discussion index requires preparation",
            ));
        }
        // A pod must not enter endpoint routing while a fresh process would
        // reject Git reads and trigger shared index maintenance on first use.
        repository
            .open_current(server, server.options, &server.cancellation)
            .await?;
    }
    Ok(())
}

fn readiness_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", "5")],
        Json(json!({"status":"unavailable"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn scale_down_requests_native_handoff_and_cancels_admission() {
        let server = crate::test_support::server::fixture_without_cells().await;
        let response = router(Arc::clone(&server), Router::new())
            .oneshot(
                Request::post("/internal/cells/v1/scale-down")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let observed = (
            response.status(),
            server.cancellation.is_cancelled(),
            matches!(
                *server.shutdown_mode.borrow(),
                cellar::sdk::ShutdownMode::Handoff
            ),
        );
        crate::test_support::server::close(&server).await;
        assert_eq!(observed, (StatusCode::ACCEPTED, true, true));
    }
}
