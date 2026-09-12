use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as SyncRwLock};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use crab_metadata::manifest_store::read_manifest;
use crab_remote_git::{
    OperationLimits, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{Store, StoreLayout};
use serde_json::json;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::catalog::CatalogStore;
use crate::{
    Config, RepositoryConfig, Result, api, app, archive, assets, assignees,
    auth::{self, Authentication, Principal},
    branches, checks, contents, git, issues, labels, lfs, maintenance, pulls, receive, releases,
    repository_settings::{self, BranchProtections, RepositoryLifecycle},
    statuses,
};

pub(crate) const MAX_DEPENDENCY_FILE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct Repository {
    pub config: RepositoryConfig,
    pub store: Store,
    pub layout: StoreLayout<Store>,
    pub identity: RepositoryIdentity,
    pub(crate) protections: RwLock<BranchProtections>,
    pub(crate) lifecycle: RwLock<RepositoryLifecycle>,
    pinned: Mutex<Option<(Instant, RemoteGitRepository)>>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<crab_write::Result<()>>>>,
}

pub(crate) struct RepositorySet {
    current: SyncRwLock<BTreeMap<(String, String), Arc<Repository>>>,
}

impl RepositorySet {
    pub(crate) fn get(&self, key: &(String, String)) -> Option<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .cloned()
    }

    pub(crate) fn values(&self) -> Vec<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    fn replace(&self, next: BTreeMap<(String, String), Arc<Repository>>) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
    }

    #[cfg(test)]
    pub(crate) fn get_mut(&mut self, key: &(String, String)) -> Option<&mut Repository> {
        self.current
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(key)
            .and_then(Arc::get_mut)
    }
}

impl From<BTreeMap<(String, String), Repository>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Repository>) -> Self {
        Self {
            current: SyncRwLock::new(
                repositories
                    .into_iter()
                    .map(|(key, repository)| (key, Arc::new(repository)))
                    .collect(),
            ),
        }
    }
}

impl From<BTreeMap<(String, String), Arc<Repository>>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        Self {
            current: SyncRwLock::new(repositories),
        }
    }
}

impl Repository {
    pub(crate) async fn branch_protections(&self) -> app::Result<BranchProtections> {
        repository_settings::refresh(self).await
    }

    pub(crate) async fn lifecycle(&self) -> app::Result<RepositoryLifecycle> {
        repository_settings::refresh_lifecycle(self).await
    }

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
            Ok(repository)
                if repository.refs().is_empty() || repository.commit_graph_available() =>
            {
                return Ok(repository);
            }
            Ok(_) => {}
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
                Ok(repository)
                    if repository.refs().is_empty() || repository.commit_graph_available() =>
                {
                    return Ok(repository);
                }
                Ok(_) => {}
                Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            *worker = Some(tokio::spawn(maintenance::run(
                self.store.clone(),
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
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
    pub repositories: RepositorySet,
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
    catalog: Option<CatalogStore>,
    catalog_healthy: AtomicBool,
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

/// Serve configured repositories and compiled React assets until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let catalog = CatalogStore::from_config(&config)?;
    let (document, _) = catalog.load().await?;
    let auth = match config.auth.clone() {
        Some(config) => Some(
            Authentication::new_durable(config, catalog.root())
                .await
                .map_err(|source| crate::Error::Identity {
                    source: Box::new(source),
                })?,
        ),
        None => None,
    };
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let management_listener = tokio::net::TcpListener::bind(config.management_listen).await?;
    let port = listener.local_addr()?.port();
    let catalog_version = document.version;
    let repositories = materialize_catalog(&catalog, document).await?;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancellation = CancellationToken::new();
    let options = RepositoryOptions::new(
        Default::default(),
        OperationLimits {
            // Graph-backed batches keep deep blame bounded while covering
            // first-parent histories of Kubernetes-scale repositories.
            max_duration: Duration::from_secs(2 * 60),
            max_logical_objects: 175_000,
            max_storage_requests: 200_000,
            max_entries: 2_000_000,
            max_history_commits: 75_000,
            max_blame_comparison_cells: 64_000_000,
            max_response_bytes: 8 * 1024 * 1024,
            ..Default::default()
        },
    )?;
    let server = Arc::new(Server {
        repositories: repositories.into(),
        runtime: Arc::clone(&runtime),
        cancellation: cancellation.clone(),
        receives: tokio_util::task::TaskTracker::new(),
        options,
        cursor_key: auth
            .as_ref()
            .map(Authentication::cursor_key)
            .unwrap_or_else(rand::random),
        admission: Semaphore::new(16),
        git_admission: Arc::new(Semaphore::new(4)),
        app_admission: Semaphore::new(8),
        maintenance_admission: Arc::new(Semaphore::new(2)),
        port,
        auth,
        catalog: Some(catalog),
        catalog_healthy: AtomicBool::new(true),
    });
    let app = router(Arc::clone(&server));
    let management = management_router(Arc::clone(&server));
    tracing::info!(address = %listener.local_addr()?, "public listener started");
    tracing::info!(address = %management_listener.local_addr()?, "management listener started");
    let signal_cancellation = cancellation.clone();
    let signal = tokio::spawn(async move {
        shutdown_signal().await;
        signal_cancellation.cancel();
    });
    let refresh_server = Arc::clone(&server);
    let refresh =
        tokio::spawn(async move { refresh_catalog(refresh_server, catalog_version).await });
    let public_shutdown = cancellation.clone();
    let management_shutdown = cancellation.clone();
    let result = tokio::try_join!(
        axum::serve(listener, app).with_graceful_shutdown(public_shutdown.cancelled_owned()),
        axum::serve(management_listener, management)
            .with_graceful_shutdown(management_shutdown.cancelled_owned()),
    );
    cancellation.cancel();
    signal.abort();
    if let Err(error) = refresh.await {
        tracing::warn!(error = %error, "repository catalog refresh task failed");
    }
    // Axum has drained its connections, so no handler can register a new
    // receive after the tracker becomes empty. Close readers only after that drain.
    server.cancellation.cancel();
    server.receives.close();
    server.receives.wait().await;
    let maintenance = server.finish_maintenance().await;
    runtime.shutdown().await;
    result
        .map(|_| ())
        .map_err(crate::Error::from)
        .and(maintenance)
}

async fn materialize_catalog(
    catalog: &CatalogStore,
    document: crate::catalog::CatalogDocument,
) -> Result<BTreeMap<(String, String), Arc<Repository>>> {
    let mut repositories = BTreeMap::new();
    for record in document.repositories {
        let store = catalog.root().store.clone();
        let prefix = catalog.root().repository_prefix(&record.prefix)?;
        let layout = StoreLayout::new(store.clone(), prefix.clone());
        let (manifest, _) =
            read_manifest(&store, &layout)
                .await
                .map_err(|source| crate::Error::Settings {
                    source: Box::new(source),
                })?;
        let default_branch =
            manifest
                .head
                .strip_prefix("refs/heads/")
                .ok_or(crate::Error::Config(
                    "catalog repository HEAD must name a branch",
                ))?;
        let entry = record.runtime_config(catalog.root(), default_branch)?;
        let configured_protections = BranchProtections::configured(&entry.protected_branches);
        let repository = Repository {
            layout,
            identity: RepositoryIdentity::new(
                catalog.root().provider_namespace.clone(),
                prefix,
                record.placement_generation,
            )?,
            config: entry.clone(),
            store,
            protections: RwLock::new(configured_protections),
            lifecycle: RwLock::new(RepositoryLifecycle::active()),
            pinned: Mutex::new(None),
            maintenance: Mutex::new(None),
        };
        let protections = repository_settings::load(&repository)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        *repository.protections.write().await = protections;
        let lifecycle = repository_settings::load_lifecycle(&repository)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        *repository.lifecycle.write().await = lifecycle;
        repositories.insert(
            (entry.owner.clone(), entry.name.clone()),
            Arc::new(repository),
        );
    }
    Ok(repositories)
}

async fn refresh_catalog(server: Arc<Server>, mut version: u64) {
    let Some(catalog) = server.catalog.clone() else {
        return;
    };
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        let (document, _) = match catalog.load().await {
            Ok(value) => value,
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                tracing::warn!(error = ?error, "repository catalog refresh failed");
                continue;
            }
        };
        if document.version < version {
            server.catalog_healthy.store(false, Ordering::Release);
            tracing::warn!(
                catalog_version = document.version,
                active_version = version,
                "repository catalog version moved backwards"
            );
            continue;
        }
        if document.version == version {
            server.catalog_healthy.store(true, Ordering::Release);
            continue;
        }
        let next_version = document.version;
        match materialize_catalog(&catalog, document).await {
            Ok(repositories) => {
                server.repositories.replace(repositories);
                version = next_version;
                server.catalog_healthy.store(true, Ordering::Release);
                tracing::info!(catalog_version = version, "repository catalog refreshed");
            }
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                tracing::warn!(error = ?error, "repository catalog materialization failed");
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    eprintln!("Unable to listen for SIGTERM: {error}");
                    if let Err(error) = tokio::signal::ctrl_c().await {
                        eprintln!("Unable to listen for Ctrl-C: {error}");
                    }
                    return;
                }
            };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("Unable to listen for Ctrl-C: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("Unable to listen for Ctrl-C: {error}");
    }
}

pub(crate) fn router(server: Arc<Server>) -> Router {
    Router::new()
        .merge(assignees::routes(Arc::clone(&server)))
        .merge(branches::routes())
        .merge(checks::routes(Arc::clone(&server)))
        .merge(contents::routes())
        .merge(issues::routes(Arc::clone(&server)))
        .merge(labels::routes(Arc::clone(&server)))
        .merge(releases::routes(Arc::clone(&server)))
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
        .route("/api/repos/{owner}/{name}/archive", get(archive::download))
        .route("/api/repos/{owner}/{name}/{action}", get(api::read))
        .fallback(assets::serve)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            boundary,
        ))
        .with_state(server)
}

fn management_router(server: Arc<Server>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/readyz", get(readiness))
        .with_state(server)
}

async fn readiness(State(server): State<Arc<Server>>) -> Response {
    let check = async {
        if !server.catalog_healthy.load(Ordering::Acquire) {
            return Err(crate::Error::Config("catalog refresh is unhealthy"));
        }
        let catalog = server
            .catalog
            .as_ref()
            .ok_or(crate::Error::Config("catalog is unavailable"))?;
        catalog.load().await?;
        Ok::<_, crate::Error>(())
    };
    match tokio::time::timeout(Duration::from_secs(10), check).await {
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

fn readiness_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", "5")],
        Json(json!({"status":"unavailable"})),
    )
        .into_response()
}

async fn catalog(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> app::Result<Json<serde_json::Value>> {
    let mut repositories = Vec::new();
    for repository in server
        .repositories
        .values()
        .into_iter()
        .filter(|repository| principal.can_read(&repository.config))
    {
        let protections = repository.branch_protections().await?;
        let lifecycle = repository.lifecycle().await?;
        repositories.push(json!({
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if principal.can_write(&repository.config) { "write" } else { "read" },
            "can_admin": principal.can_admin(&repository.config),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        }));
    }
    Ok(Json(json!({"repositories":repositories})))
}

async fn boundary(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );
    async move {
        let mut response = boundary_request(server, request, next).await;
        if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
            response.headers_mut().insert("x-request-id", value);
        }
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            "request completed"
        );
        response
    }
    .instrument(span)
    .await
}

async fn boundary_request(server: Arc<Server>, mut request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok());
    let allowed = [
        format!("127.0.0.1:{}", server.port),
        format!("localhost:{}", server.port),
        format!("[::1]:{}", server.port),
    ];
    let local_host = allowed.iter().any(|value| Some(value.as_str()) == host);
    let health_probe = matches!(request.uri().path(), "/healthz" | "/readyz");
    let valid_host = (health_probe && local_host)
        || server
            .auth
            .as_ref()
            .map(|auth| auth.allows_host(host))
            .unwrap_or(local_host);
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
    let archived_response = if !denied && !rejected_mutation && unsafe_method && !git_request {
        archived_mutation_response(&server, &principal, request.uri().path()).await
    } else {
        None
    };
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
    } else if let Some(response) = archived_response {
        response
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
            ("content-security-policy", "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; img-src 'self' data: blob:; media-src 'self' blob:; frame-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"),
        ], response,
    ).into_response()
}

async fn archived_mutation_response(
    server: &Server,
    principal: &Principal,
    path: &str,
) -> Option<Response> {
    let mut segments = path.strip_prefix("/api/repos/")?.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    segments.next()?;
    if owner.is_empty()
        || name.is_empty()
        || path == format!("/api/repos/{owner}/{name}/settings/archive")
    {
        return None;
    }
    let repository = server
        .repositories
        .get(&(owner.to_owned(), name.to_owned()))
        .filter(|repository| principal.can_read(&repository.config))?;
    match repository.lifecycle().await {
        Ok(lifecycle) if lifecycle.archived => Some(app::Error::Archived.into_response()),
        Ok(_) => None,
        Err(error) => Some(error.into_response()),
    }
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
        let server = Arc::new(Server {
            repositories: RepositorySet::from(BTreeMap::<(String, String), Repository>::new()),
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
            catalog: None,
            catalog_healthy: AtomicBool::new(false),
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
            if expected == StatusCode::NOT_FOUND {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["error"]["code"], "repository_not_found");
            }
        }
        for path in ["/healthz", "/readyz"] {
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
        let management = management_router(server);
        for (path, expected) in [
            ("/healthz", StatusCode::OK),
            ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = management
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
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
