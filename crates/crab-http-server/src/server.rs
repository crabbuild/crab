use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crab_remote_git::{
    OperationLimits, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{StorageProviderKind, Store, StoreLayout, build_static_env_store};
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{
    Config, RepositoryConfig, Result, api, assets, assignees,
    auth::{self, Authentication, Principal},
    checks, git, issues, labels, lfs, maintenance, pulls, receive, statuses,
};

pub(crate) const MAX_DEPENDENCY_FILE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct Repository {
    pub config: RepositoryConfig,
    pub store: Store,
    pub layout: StoreLayout<Store>,
    pub identity: RepositoryIdentity,
    pinned: Mutex<Option<(Instant, RemoteGitRepository)>>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<crab_write::Result<()>>>>,
}

impl Repository {
    pub(crate) async fn invalidate(&self) {
        *self.pinned.lock().await = None;
    }

    pub async fn open(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitRepository> {
        let mut pinned = tokio::select! {
            _ = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            pinned = self.pinned.lock() => pinned,
        };
        if let Some((checked, repository)) = pinned.as_ref()
            && checked.elapsed() < Duration::from_secs(2)
        {
            return Ok(repository.clone());
        }
        // Journal commits can change refs without changing the manifest ETag.
        // Reopen after the cache window so pending publication becomes visible.
        let repository = self
            .open_current(server, server.options, cancellation)
            .await?;
        *pinned = Some((Instant::now(), repository.clone()));
        Ok(repository)
    }

    pub(crate) async fn open_current(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitRepository> {
        let open = || {
            RemoteGitRepository::open(
                self.store.clone(),
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
                cancellation,
            )
        };
        match open().await {
            Ok(repository) => return Ok(repository),
            Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let mut worker = tokio::select! {
            () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            worker = self.maintenance.lock() => worker,
        };
        if worker.is_none() {
            // A preceding request may have finished maintenance while this one waited.
            match open().await {
                Ok(repository) => return Ok(repository),
                Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            *worker = Some(tokio::spawn(maintenance::run(
                self.store.clone(),
                self.layout.clone(),
                Arc::clone(&server.maintenance_admission),
                server.cancellation.clone(),
            )));
        }
        if let Some(task) = worker.as_mut() {
            // A cancelled reader leaves the handle in this slot. A later reader
            // or server shutdown must drain publication and its lease cleanup.
            let result = tokio::select! {
                () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
                result = task => result,
            };
            *worker = None;
            result??;
        }
        open().await.map_err(Into::into)
    }
}

pub(crate) struct Server {
    pub repositories: BTreeMap<(String, String), Repository>,
    pub runtime: Arc<RemoteGitRuntime>,
    pub options: RepositoryOptions,
    pub cursor_key: [u8; 32],
    pub admission: Semaphore,
    pub git_admission: Arc<Semaphore>,
    pub app_admission: Semaphore,
    maintenance_admission: Arc<Semaphore>,
    pub cancellation: CancellationToken,
    pub receives: tokio_util::task::TaskTracker,
    port: u16,
    pub auth: Option<Authentication>,
}

impl Server {
    async fn finish_maintenance(&self) -> Result<()> {
        let mut result = Ok(());
        for repository in self.repositories.values() {
            if let Some(task) = repository.maintenance.lock().await.take() {
                let completed = match task.await {
                    Ok(Ok(())) | Ok(Err(crab_write::WriteError::Cancelled)) => Ok(()),
                    Ok(Err(error)) => Err(crate::Error::from(error)),
                    Err(error) => Err(crate::Error::from(error)),
                };
                result = result.and(completed);
            }
        }
        result
    }
}

/// Serve configured repositories and compiled React assets until Ctrl-C.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let auth =
        match config.auth {
            Some(config) => Some(Authentication::new(config).await.map_err(|source| {
                crate::Error::Identity {
                    source: Box::new(source),
                }
            })?),
            None => None,
        };
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
            maintenance: Mutex::new(None),
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
        receives: tokio_util::task::TaskTracker::new(),
        options,
        cursor_key: rand::random(),
        admission: Semaphore::new(16),
        git_admission: Arc::new(Semaphore::new(4)),
        app_admission: Semaphore::new(8),
        maintenance_admission: Arc::new(Semaphore::new(2)),
        port,
        auth,
    });
    let app = router(Arc::clone(&server));
    println!("Crab repositories: http://{}", listener.local_addr()?);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if tokio::signal::ctrl_c().await.is_err() {
                eprintln!("Unable to listen for shutdown signal");
            }
            cancellation.cancel();
        })
        .await;
    // Axum has drained its connections, so no handler can register a new
    // receive after the tracker becomes empty. Close readers only after that drain.
    server.cancellation.cancel();
    server.receives.close();
    server.receives.wait().await;
    let maintenance = server.finish_maintenance().await;
    runtime.shutdown().await;
    result.map_err(crate::Error::from).and(maintenance)
}

pub(crate) fn router(server: Arc<Server>) -> Router {
    Router::new()
        .merge(assignees::routes(Arc::clone(&server)))
        .merge(checks::routes(Arc::clone(&server)))
        .merge(issues::routes(Arc::clone(&server)))
        .merge(labels::routes(Arc::clone(&server)))
        .merge(pulls::routes(Arc::clone(&server)))
        .merge(statuses::routes(Arc::clone(&server)))
        .route(
            "/git/{owner}/{name}/info/lfs/objects/batch",
            post(lfs::batch).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/objects/{oid}",
            get(lfs::download).put(lfs::upload),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/locks/verify",
            post(lfs::locks_unavailable),
        )
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/git/{owner}/{name}/info/refs", get(git::advertise))
        .route(
            "/git/{owner}/{name}/git-receive-pack",
            post(receive::receive),
        )
        .route(
            "/git/{owner}/{name}/git-upload-pack",
            post(git::upload_pack).layer(axum::extract::DefaultBodyLimit::max(git::MAX_BODY_BYTES)),
        )
        .route(
            "/api/git-token",
            post(auth::issue_git_token)
                .delete(auth::revoke_git_tokens)
                .layer(axum::extract::DefaultBodyLimit::max(2048)),
        )
        .route("/api/session", get(auth::session))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route("/api/repos", get(catalog))
        .route("/api/repos/{owner}/{name}/{action}", get(api::read))
        .fallback(assets::serve)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            boundary,
        ))
        .with_state(server)
}

async fn catalog(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    Json(
        json!({"repositories": server.repositories.values().filter(|repository| principal.can_read(&repository.config)).map(|repository| json!({
        "owner": repository.config.owner, "name": repository.config.name,
        "description": repository.config.description,
        "access": if principal.can_write(&repository.config) { "write" } else { "read" },
        "protected_branches": repository.config.protected_branches,
    })).collect::<Vec<_>>()}),
    )
}

async fn boundary(State(server): State<Arc<Server>>, mut request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok());
    let allowed = [
        format!("127.0.0.1:{}", server.port),
        format!("localhost:{}", server.port),
        format!("[::1]:{}", server.port),
    ];
    let valid_host = server
        .auth
        .as_ref()
        .map(|auth| auth.allows_host(host))
        .unwrap_or_else(|| allowed.iter().any(|value| Some(value.as_str()) == host));
    if !valid_host {
        return StatusCode::FORBIDDEN.into_response();
    }
    let git_request = request.uri().path().starts_with("/git/");
    let integration_request = integration_api_path(request.uri().path());
    let token_request = integration_request && request.headers().contains_key("authorization");
    let principal = match &server.auth {
        Some(auth) if git_request || token_request => auth.git_principal(request.headers()).await,
        Some(auth) => auth.principal(request.headers()).await,
        None => Principal::Local,
    };
    let protected =
        request.uri().path().starts_with("/api/") && request.uri().path() != "/api/session";
    let denied = (protected || git_request) && !principal.authenticated();
    let unsafe_method = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let rejected_mutation = !git_request
        && unsafe_method
        && !matches!(principal, Principal::Git(_))
        && server
            .auth
            .as_ref()
            .is_some_and(|auth| !auth.accepts_mutation(&principal, request.headers()));
    request.extensions_mut().insert(principal);
    let mut response = if denied && git_request {
        (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                "Basic realm=\"Crab Git\", charset=\"UTF-8\"",
            )],
            "Use a Git access token from your signed-in Crab account",
        )
            .into_response()
    } else if denied {
        (StatusCode::UNAUTHORIZED, Json(json!({"error":{"code":"sign_in_required","message":"Sign in to access repositories"}}))).into_response()
    } else if rejected_mutation {
        (StatusCode::FORBIDDEN, Json(json!({"error":{"code":"csrf_rejected","message":"Reload the page before trying again"}}))).into_response()
    } else {
        next.run(request).await
    };
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

fn integration_api_path(path: &str) -> bool {
    let mut segments = path.split('/');
    segments.next() == Some("")
        && segments.next() == Some("api")
        && segments.next() == Some("repos")
        && segments.next().is_some_and(|value| !value.is_empty())
        && segments.next().is_some_and(|value| !value.is_empty())
        && match segments.next() {
            Some("check-runs") => segments.next().is_none(),
            Some("statuses") => {
                segments.next().is_some_and(|value| !value.is_empty()) && segments.next().is_none()
            }
            Some("commits") => {
                segments.next().is_some_and(|value| !value.is_empty())
                    && match segments.next() {
                        Some("status") => segments.next().is_none(),
                        Some("check-runs") => segments
                            .next()
                            .is_none_or(|value| !value.is_empty() && segments.next().is_none()),
                        _ => false,
                    }
            }
            _ => false,
        }
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod maintenance_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn repository_tokens_are_only_considered_on_exact_integration_routes() {
        assert!(integration_api_path(
            "/api/repos/team/repo/statuses/0123456789012345678901234567890123456789"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/status"
        ));
        assert!(integration_api_path("/api/repos/team/repo/check-runs"));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs/1"
        ));
        for path in [
            "/api/repos/team/repo/pulls/1",
            "/api/repos/team/repo/check-runs/1",
            "/api/repos/team/repo/statuses/oid/extra",
            "/api/repos/team/repo/commits/oid/statuses",
            "/api/repos/team/repo/commits/oid/check-runs/1/extra",
            "/api/repos/team/repo/commits/oid/check-runs/",
            "/api/repos//repo/commits/oid/statuses",
            "/api/repos/team/repo/commits//statuses",
        ] {
            assert!(!integration_api_path(path), "{path}");
        }
    }

    #[tokio::test]
    async fn transport_enforces_host_and_preserves_asset_cache_policy() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let app = router(Arc::new(Server {
            repositories: BTreeMap::new(),
            runtime: Arc::clone(&runtime),
            options: RepositoryOptions::default(),
            cursor_key: [0; 32],
            admission: Semaphore::new(1),
            git_admission: Arc::new(Semaphore::new(1)),
            app_admission: Semaphore::new(1),
            maintenance_admission: Arc::new(Semaphore::new(1)),
            cancellation: CancellationToken::new(),
            receives: tokio_util::task::TaskTracker::new(),
            port: 8788,
            auth: None,
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

#[cfg(test)]
#[path = "receive_fault_tests.rs"]
mod receive_fault_tests;

#[cfg(test)]
#[path = "receive_tests.rs"]
mod receive_tests;

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;

#[cfg(test)]
#[path = "lfs_tests.rs"]
mod lfs_tests;

#[cfg(test)]
#[path = "pulls_tests.rs"]
mod pulls_tests;
