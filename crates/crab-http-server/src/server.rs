use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::{
    Json, Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_remote_git::{
    OperationLimits, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{StorageProviderKind, Store, StoreLayout, build_static_env_store};
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{Config, RepositoryConfig, Result, api, assets};

pub(crate) struct Repository {
    pub config: RepositoryConfig,
    pub store: Store,
    pub layout: StoreLayout<Store>,
    pub identity: RepositoryIdentity,
    pinned: Mutex<Option<(Instant, RemoteGitRepository)>>,
}

impl Repository {
    pub async fn open(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> crab_remote_git::Result<RemoteGitRepository> {
        let mut pinned = tokio::select! {
            _ = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled),
            pinned = self.pinned.lock() => pinned,
        };
        if let Some((checked, repository)) = pinned.as_mut() {
            if checked.elapsed() < Duration::from_secs(2) {
                return Ok(repository.clone());
            }
            if repository.is_current(cancellation).await? {
                *checked = Instant::now();
                return Ok(repository.clone());
            }
        }
        let repository = RemoteGitRepository::open(
            self.store.clone(),
            self.layout.clone(),
            self.identity.clone(),
            Arc::clone(&server.runtime),
            server.options,
            cancellation,
        )
        .await?;
        *pinned = Some((Instant::now(), repository.clone()));
        Ok(repository)
    }
}

pub(crate) struct Server {
    pub repositories: BTreeMap<(String, String), Repository>,
    pub runtime: Arc<RemoteGitRuntime>,
    pub options: RepositoryOptions,
    pub cursor_key: [u8; 32],
    pub admission: Semaphore,
    pub cancellation: CancellationToken,
    port: u16,
}

/// Serve configured repositories and compiled React assets until Ctrl-C.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let port = listener.local_addr()?.port();
    let mut repositories = BTreeMap::new();
    for entry in config.repositories {
        let store = build_static_env_store(&entry.bucket, StorageProviderKind::S3)?;
        let repository = Repository {
            layout: StoreLayout::new(store.clone(), entry.prefix.clone()),
            identity: RepositoryIdentity::new(
                format!("s3:{}", entry.bucket),
                entry.prefix.clone(),
                1,
            )?,
            config: entry.clone(),
            store,
            pinned: Mutex::new(None),
        };
        repositories.insert((entry.owner, entry.name), repository);
    }
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancellation = CancellationToken::new();
    let options = RepositoryOptions::new(
        Default::default(),
        OperationLimits {
            max_duration: Duration::from_secs(30),
            max_response_bytes: 8 * 1024 * 1024,
            ..Default::default()
        },
    )?;
    let server = Arc::new(Server {
        repositories,
        runtime: Arc::clone(&runtime),
        cancellation: cancellation.clone(),
        options,
        cursor_key: rand::random(),
        admission: Semaphore::new(16),
        port,
    });
    let app = router(server);
    println!("Crab repositories: http://{}", listener.local_addr()?);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if tokio::signal::ctrl_c().await.is_err() {
                eprintln!("Unable to listen for shutdown signal");
            }
            cancellation.cancel();
        })
        .await;
    runtime.shutdown().await;
    result.map_err(crate::Error::from)
}

fn router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/api/repos", get(catalog))
        .route("/api/repos/{owner}/{name}/{action}", get(api::read))
        .fallback(assets::serve)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            boundary,
        ))
        .with_state(server)
}

async fn catalog(State(server): State<Arc<Server>>) -> Json<serde_json::Value> {
    Json(
        json!({"repositories": server.repositories.values().map(|repository| json!({
        "owner": repository.config.owner, "name": repository.config.name,
        "description": repository.config.description,
    })).collect::<Vec<_>>()}),
    )
}

async fn boundary(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok());
    let allowed = [
        format!("127.0.0.1:{}", server.port),
        format!("localhost:{}", server.port),
        format!("[::1]:{}", server.port),
    ];
    if !allowed.iter().any(|value| Some(value.as_str()) == host) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry("cache-control")
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    (
        [
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "same-origin"),
            ("content-security-policy", "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; img-src 'self' data: blob:; base-uri 'none'; frame-ancestors 'none'"),
        ], response,
    ).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn transport_enforces_host_and_preserves_asset_cache_policy() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let app = router(Arc::new(Server {
            repositories: BTreeMap::new(),
            runtime: Arc::clone(&runtime),
            options: RepositoryOptions::default(),
            cursor_key: [0; 32],
            admission: Semaphore::new(1),
            cancellation: CancellationToken::new(),
            port: 8788,
        }));
        for (path, host, expected, cache) in [
            (
                "/api/repos",
                "untrusted.invalid",
                StatusCode::FORBIDDEN,
                None,
            ),
            (
                "/api/repos",
                "127.0.0.1:8788",
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
        ] {
            let request = Request::builder()
                .uri(path)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{path}, {host}");
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok()),
                cache
            );
            if expected == StatusCode::NOT_FOUND {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["error"]["code"], "repository_not_found");
            }
        }
        runtime.shutdown().await;
    }
}
