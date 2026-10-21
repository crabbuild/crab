//! Public transport composition and the shared security boundary.

use super::Server;
use crate::{api, assets, auth, git, git_import};
use axum::{Json, Router, middleware, routing::get};
use serde_json::json;
use std::sync::Arc;

mod boundary;

pub(crate) fn router(server: Arc<Server>) -> Router {
    Router::new()
        .merge(api::routes(Arc::clone(&server)))
        .merge(git_import::routes(Arc::clone(&server)))
        .merge(git::routes())
        .merge(auth::routes())
        .route("/livez", get(|| async { Json(json!({"status": "ok"})) }))
        .fallback(assets::serve)
        // Wrap every domain router and fallback in the same identity/mutation
        // boundary; applying this inside one owner would leave siblings exposed.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            boundary::protect,
        ))
        .with_state(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        repository::{
            Repository,
            catalog::{RepositoryIndex, RepositorySet},
        },
        transfer_admission::TransferAdmission,
    };

    use axum::{body::Body, extract::Request, http::StatusCode};
    use crab_remote_git::RemoteGitRuntime;
    use crab_storage::Store;
    use http_body_util::BodyExt;
    use std::collections::BTreeMap;
    use tokio::sync::Semaphore;

    use tower::ServiceExt;
    use uuid::Uuid;
    #[tokio::test]
    async fn scoped_catalog_skips_unrelated_cells() {
        let server = crate::test_support::server::fixture().await;
        let existing = server.repositories.values().pop().unwrap();
        let mut config = existing.config.clone();
        config.owner = "other".into();
        config.name = "unavailable".into();
        let unavailable = Arc::new(Repository::new(
            Uuid::from_bytes([2; 16]),
            config,
            existing.layout.clone(),
            existing.identity.clone(),
        ));
        server.repositories.replace(RepositoryIndex::new(
            1,
            BTreeMap::from([
                (("team".into(), "repo".into()), existing),
                (("other".into(), "unavailable".into()), unavailable),
            ]),
        ));
        let response = router(Arc::clone(&server))
            .oneshot(
                Request::builder()
                    .uri("/api/repos?owner=team&name=repo")
                    .header("host", "localhost:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        crate::test_support::server::close(&server).await;
        assert_eq!(
            (status, value["repositories"].as_array().map(Vec::len)),
            (StatusCode::OK, Some(1))
        );
    }

    #[tokio::test]
    async fn transport_enforces_host_and_preserves_asset_cache_policy() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let server = Arc::new({
            let mut server = Server::for_test(
                RepositorySet::from(BTreeMap::<(String, String), Repository>::new()),
                TransferAdmission::new(
                    Store::new(Arc::new(object_store::memory::InMemory::new())),
                    "test/.crab/http-server/v1/admission".into(),
                    1,
                ),
            );
            server.runtime = Arc::clone(&runtime);
            server.admission = Semaphore::new(1);
            server.app_admission = Semaphore::new(1);
            server.maintenance_admission = Arc::new(Semaphore::new(1));
            server
        });
        let app = router(Arc::clone(&server));
        for (path, host, expected, cache) in [
            (
                "/api/repos",
                "untrusted.invalid",
                StatusCode::FORBIDDEN,
                None,
            ),
            (
                "/api/repos",
                "127.0.0.1:18791",
                StatusCode::OK,
                Some("no-store"),
            ),
            (
                "/team/repo.name",
                "localhost:8788",
                StatusCode::OK,
                Some("no-cache"),
            ),
            (
                "/api/repos/team/missing/tree",
                "[::1]:8788",
                StatusCode::NOT_FOUND,
                Some("no-store"),
            ),
            (
                "/api/repos",
                "127.0.0.1.evil.invalid:8788",
                StatusCode::FORBIDDEN,
                None,
            ),
        ] {
            let request = Request::builder()
                .uri(path)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{path}, {host}");
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok());
            assert!(
                request_id.is_some_and(|value| Uuid::parse_str(value).is_ok()),
                "{path}, {host}"
            );
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok()),
                cache
            );
            if expected.is_success() {
                let policy = response
                    .headers()
                    .get("content-security-policy")
                    .and_then(|value| value.to_str().ok())
                    .unwrap();
                assert!(
                    policy.contains("connect-src 'self' https://extensions.duckdb.org"),
                    "{path}, {host}"
                );
            }
            if expected == StatusCode::NOT_FOUND {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["error"]["code"], "repository_not_found");
            }
        }
        for path in ["/healthz", "/readyz", "/integrityz"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:8788")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let management = crate::server::management::router(Arc::clone(&server), Router::new());
        for (path, expected) in [
            ("/healthz", StatusCode::OK),
            ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
            ("/integrityz", StatusCode::OK),
        ] {
            let response = management
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/capacity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let response = management
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("crab_server_catalog_healthy 0"));
        assert!(body.contains("crab_server_requests_total{method=\"get\",outcome=\"2xx\"} 2"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/livez")
                    .header("host", "10.42.3.17:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        server.shutdown_runtimes().await.unwrap();
    }
}
