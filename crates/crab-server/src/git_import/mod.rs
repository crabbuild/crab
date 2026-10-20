mod git;
mod source;
use git::{configure_git, default_branch, mirror_refs, mirror_size, push_mirror, run_git};
use source::{GitImportRequest, ValidatedImport, validate_request};

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    extract::{Path as AxumPath, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::json;
use tokio::{process::Command, sync::RwLock};
use uuid::Uuid;

use crate::{
    Config, RepositoryAccess, RepositoryMember, app,
    auth::{Identity, Principal},
    catalog::CatalogStore,
    repository::MaintenanceTrigger,
    server::Server,
};

const MAX_ACTIVE_JOBS: usize = 4;
const MAX_RETAINED_JOBS: usize = 256;
const JOB_RETENTION: Duration = Duration::from_secs(60 * 60);
const MAX_IMPORT_REFS: usize = 100_000;
const MAX_IMPORT_MIRROR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const IMPORT_MIRROR_RESERVE_MARGIN: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct ImportContext {
    config: Config,
    address: SocketAddr,
    internal_key: [u8; 32],
    allowed_hosts: Arc<[String]>,
    jobs: ImportJobs,
    importing: Arc<std::sync::Mutex<HashSet<Uuid>>>,
}

impl ImportContext {
    pub(crate) fn new(config: Config, address: SocketAddr) -> Self {
        let allowed_hosts = config
            .import
            .allowed_hosts
            .iter()
            .map(|host| host.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .into();
        Self {
            config,
            address,
            internal_key: rand::random(),
            allowed_hosts,
            jobs: ImportJobs::default(),
            importing: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    pub(crate) fn is_repository_importing(&self, repository: Uuid) -> bool {
        self.importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&repository)
    }

    fn begin_repository(&self, repository: Uuid) -> ImportGuard {
        self.importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(repository);
        ImportGuard {
            importing: Arc::clone(&self.importing),
            repository,
        }
    }

    pub(crate) fn authorizes_git(&self, request: &axum::extract::Request) -> bool {
        if !is_import_git_path(request.method(), request.uri().path()) {
            return false;
        }
        let Some(value) = request.headers().get("x-crab-internal-import") else {
            return false;
        };
        let Ok(value) = value.to_str() else {
            return false;
        };
        value == self.internal_key_hex()
            && self.host_matches(
                request
                    .headers()
                    .get("host")
                    .and_then(|value| value.to_str().ok()),
            )
    }

    fn internal_key_hex(&self) -> String {
        blake3::Hash::from_bytes(self.internal_key)
            .to_hex()
            .to_string()
    }

    fn destination_url(&self, owner: &str, name: &str) -> String {
        let host = match self.address.ip() {
            std::net::IpAddr::V4(address) if address.is_unspecified() => "127.0.0.1".to_owned(),
            std::net::IpAddr::V6(address) if address.is_unspecified() => "[::1]".to_owned(),
            address => match address {
                std::net::IpAddr::V4(address) => address.to_string(),
                std::net::IpAddr::V6(address) => format!("[{address}]"),
            },
        };
        format!(
            "http://{host}:{}/git/{}/{}.git",
            self.address.port(),
            encode_path_segment(owner),
            encode_path_segment(name)
        )
    }

    fn host_matches(&self, host: Option<&str>) -> bool {
        let Some(host) = host else {
            return false;
        };
        let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
            return false;
        };
        let expected_port = self.address.port();
        if authority.port_u16() != Some(expected_port) {
            return false;
        }
        let hostname = authority.host();
        let expected = match self.address.ip() {
            std::net::IpAddr::V4(address) if address.is_unspecified() => {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            }
            std::net::IpAddr::V6(address) if address.is_unspecified() => {
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            }
            address => address,
        };
        hostname
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address == expected)
    }
}

struct ImportGuard {
    importing: Arc<std::sync::Mutex<HashSet<Uuid>>>,
    repository: Uuid,
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        self.importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.repository);
    }
}

#[derive(Clone, Default)]
struct ImportJobs {
    entries: Arc<RwLock<HashMap<Uuid, Job>>>,
}

#[derive(Clone)]
struct Job {
    actor: String,
    created_at: Instant,
    status: GitImportStatus,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Serialize)]
struct GitImportStatus {
    id: Uuid,
    source: String,
    owner: String,
    name: String,
    state: JobState,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<ImportedRepository>,
}

#[derive(Clone, Serialize)]
struct ImportedRepository {
    owner: String,
    name: String,
}

#[derive(Debug, thiserror::Error)]
enum ImportError {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("the import service is unavailable")]
    Unavailable,
    #[error("the import job was not found")]
    NotFound,
    #[error("the import queue is busy")]
    Busy,
    #[error("import staging failed")]
    Staging(#[source] crate::local_disk::Error),
    #[error("an import for this repository is already running")]
    Conflict,
    #[error("the import was cancelled")]
    Cancelled,
    #[error("the import timed out")]
    Timeout,
    #[error("git command failed: {0}")]
    Git(String),
    #[error("repository setup failed")]
    Setup(#[source] crate::Error),
    #[error("repository catalog failed")]
    Catalog(#[source] crate::catalog::CatalogError),
    #[error("temporary import storage failed")]
    Io(#[source] std::io::Error),
}

impl IntoResponse for ImportError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", message),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "import_unavailable",
                "Git import is unavailable on this server",
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Import job not found"),
            Self::Busy | Self::Staging(crate::local_disk::Error::Busy(_)) => (
                StatusCode::TOO_MANY_REQUESTS,
                "import_busy",
                "Too many imports are running; try again shortly",
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "import_conflict",
                "An import for this repository is already running",
            ),
            Self::Cancelled
            | Self::Timeout
            | Self::Staging(crate::local_disk::Error::Cancelled) => (
                StatusCode::REQUEST_TIMEOUT,
                "import_cancelled",
                "The import did not finish; retry it to continue",
            ),
            Self::Git(_) => (
                StatusCode::BAD_GATEWAY,
                "git_source_unavailable",
                "The Git source could not be cloned or published; verify the URL, access, token, and server capacity",
            ),
            Self::Staging(crate::local_disk::Error::TooLarge) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "import_capacity",
                "Git source exceeds the node disk capacity",
            ),
            Self::Setup(_) | Self::Catalog(_) | Self::Io(_) | Self::Staging(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "import_failed",
                "The repository could not be prepared for import",
            ),
        };
        if !matches!(
            self,
            Self::Invalid(_) | Self::NotFound | Self::Busy | Self::Conflict
        ) {
            tracing::error!(error = ?self, "Git import request failed");
        }
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/imports/git", post(start))
        .route("/api/imports/git/{id}", get(status))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn start(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    body: Result<Json<GitImportRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<GitImportStatus>), ImportError> {
    let context = server.git_import.as_ref().ok_or(ImportError::Unavailable)?;
    let actor = app::actor(&principal).map_err(|_| ImportError::Invalid("sign-in required"))?;
    let Json(request) = body.map_err(|_| ImportError::Invalid("invalid import request"))?;
    let request = validate_request(request, &context.allowed_hosts)?;
    let actor_key = actor_key(&actor);
    let status = context.jobs.enqueue(actor_key, &request).await?;
    let job_id = status.id;
    let worker_server = Arc::clone(&server);
    let worker_context = context.clone();
    server.receives.spawn(async move {
        run_import(worker_server, worker_context, job_id, request, actor).await;
    });
    Ok((StatusCode::ACCEPTED, Json(status)))
}

async fn status(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    AxumPath(id): AxumPath<Uuid>,
) -> Result<Json<GitImportStatus>, ImportError> {
    let context = server.git_import.as_ref().ok_or(ImportError::Unavailable)?;
    let actor = app::actor(&principal).map_err(|_| ImportError::NotFound)?;
    context.jobs.status(&actor_key(&actor), id).await.map(Json)
}

impl ImportJobs {
    async fn enqueue(
        &self,
        actor: String,
        request: &ValidatedImport,
    ) -> Result<GitImportStatus, ImportError> {
        let mut entries = self.entries.write().await;
        let now = Instant::now();
        entries.retain(|_, job| {
            !matches!(job.status.state, JobState::Succeeded | JobState::Failed)
                || now.duration_since(job.created_at) < JOB_RETENTION
        });
        let active = entries
            .values()
            .filter(|job| matches!(job.status.state, JobState::Queued | JobState::Running))
            .count();
        if entries.values().any(|job| {
            matches!(job.status.state, JobState::Queued | JobState::Running)
                && job.status.owner.eq_ignore_ascii_case(&request.owner)
                && job.status.name.eq_ignore_ascii_case(&request.name)
        }) {
            return Err(ImportError::Conflict);
        }
        if active >= MAX_ACTIVE_JOBS || entries.len() >= MAX_RETAINED_JOBS {
            return Err(ImportError::Busy);
        }
        let id = Uuid::now_v7();
        let status = GitImportStatus {
            id,
            source: request.source.clone(),
            owner: request.owner.clone(),
            name: request.name.clone(),
            state: JobState::Queued,
            message: "Queued for import".into(),
            repository: None,
        };
        entries.insert(
            id,
            Job {
                actor,
                created_at: now,
                status: status.clone(),
            },
        );
        Ok(status)
    }

    async fn status(&self, actor: &str, id: Uuid) -> Result<GitImportStatus, ImportError> {
        let entries = self.entries.read().await;
        let job = entries.get(&id).filter(|job| job.actor == actor);
        job.map(|job| job.status.clone())
            .ok_or(ImportError::NotFound)
    }

    async fn update(&self, id: Uuid, state: JobState, message: String) {
        if let Some(job) = self.entries.write().await.get_mut(&id) {
            job.status.state = state;
            job.status.message = message;
        }
    }

    async fn succeed(&self, id: Uuid, repository: ImportedRepository) {
        if let Some(job) = self.entries.write().await.get_mut(&id) {
            job.status.state = JobState::Succeeded;
            job.status.message = "Repository imported; indexing in background".into();
            job.status.repository = Some(repository);
        }
    }
}

async fn run_import(
    server: Arc<Server>,
    context: ImportContext,
    id: Uuid,
    request: ValidatedImport,
    actor: Identity,
) {
    context
        .jobs
        .update(id, JobState::Running, "Cloning from Git".into())
        .await;
    let result = import_repository(&server, &context, id, &request, &actor).await;
    match result {
        Ok(()) => {
            context
                .jobs
                .succeed(
                    id,
                    ImportedRepository {
                        owner: request.owner,
                        name: request.name,
                    },
                )
                .await;
        }
        Err(error) => {
            let message = match &error {
                ImportError::Git(_) => {
                    "The Git source could not be cloned or published; verify the URL, access, token, and server capacity".into()
                }
                ImportError::Cancelled | ImportError::Timeout => {
                    "The import did not finish; retry it to continue".into()
                }
                ImportError::Busy => "Import storage is busy; retry shortly".into(),
                ImportError::Catalog(crate::catalog::CatalogError::Conflict) => {
                    "A repository with this owner and name already exists".into()
                }
                _ => "The repository could not be prepared for import".into(),
            };
            context.jobs.update(id, JobState::Failed, message).await;
            tracing::error!(error = ?error, "Git import job failed");
        }
    }
}

async fn import_repository(
    server: &Arc<Server>,
    context: &ImportContext,
    id: Uuid,
    request: &ValidatedImport,
    actor: &Identity,
) -> Result<(), ImportError> {
    let catalog = CatalogStore::from_config(&context.config).map_err(ImportError::Setup)?;
    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    if document.repositories.iter().any(|record| {
        record.owner.eq_ignore_ascii_case(&request.owner)
            && record.name.eq_ignore_ascii_case(&request.name)
    }) {
        return Err(ImportError::Catalog(crate::catalog::CatalogError::Conflict));
    }

    let cancellation = server.cancellation.child_token();
    // Leave the receiver's initial pack reservation available while cloning.
    // A fixed 8 GiB reservation rejects small imports on a constrained node
    // before the mirror size is known, even though the mirror would fit.
    let clone_reservation = MAX_IMPORT_MIRROR_BYTES.min(
        server
            .local_staging
            .available_bytes()
            .saturating_sub(crate::git::receive::IMPORT_RECEIVE_INITIAL_RESERVATION),
    );
    if clone_reservation == 0 {
        return Err(ImportError::Busy);
    }
    let directory = match server
        .local_staging
        .create(clone_reservation, &cancellation)
        .await
    {
        Ok(directory) => directory,
        Err(error) => return Err(ImportError::Staging(error)),
    };
    let mirror = directory.path().join("repository.git");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--mirror", "--quiet", "--"])
        .arg(&request.source)
        .arg(&mirror);
    configure_git(&mut clone, request.token.as_deref(), None);
    run_git(clone, &cancellation, request.token.as_deref()).await?;

    // The upper bound protects the clone while it is being created, but keeping
    // that reservation during publication double-counts the local mirror and
    // incoming receive pack. Shrink it to the measured mirror plus a small
    // command margin before the first internal push so normal-sized imports can
    // share the node's bounded staging budget with the receiver.
    let mirror_bytes = mirror_size(&mirror).await?;
    let mirror_reservation = mirror_bytes
        .checked_add(IMPORT_MIRROR_RESERVE_MARGIN)
        .filter(|reservation| *reservation <= MAX_IMPORT_MIRROR_BYTES)
        .ok_or_else(|| ImportError::Git("Git source exceeds the import storage limit".into()))?;
    server
        .local_staging
        .resize(&directory, mirror_reservation)
        .map_err(ImportError::Staging)?;

    let default_branch = default_branch(&mirror, &cancellation).await?;
    context
        .jobs
        .update(id, JobState::Running, "Validating Git history".into())
        .await;
    let refs = mirror_refs(&mirror, &cancellation).await?;
    if refs.is_empty() {
        return Err(ImportError::Git("Git source has no refs".into()));
    }
    if refs.len() > MAX_IMPORT_REFS {
        return Err(ImportError::Git(format!(
            "Git source has too many refs (maximum {MAX_IMPORT_REFS})"
        )));
    }
    context
        .jobs
        .update(id, JobState::Running, "Preparing Crab repository".into())
        .await;
    let members = if context.config.auth.is_some() {
        vec![RepositoryMember {
            subject: actor.subject.clone(),
            name: actor.name.clone(),
            access: RepositoryAccess::Admin,
        }]
    } else {
        Vec::new()
    };
    let record = catalog
        .create_repository_exclusive(
            request.owner.clone(),
            request.name.clone(),
            format!("{}/{}", request.owner, request.name),
            default_branch,
            request.description.clone(),
            members,
        )
        .await
        .map_err(ImportError::Catalog)?;
    let maintenance_guard = context.begin_repository(record.id);
    crate::cells::initialize_repository(&context.config, record.id)
        .await
        .map_err(ImportError::Setup)?;

    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    server
        .install_catalog(document)
        .await
        .map_err(ImportError::Setup)?;

    context
        .jobs
        .update(id, JobState::Running, "Publishing Git history".into())
        .await;
    let destination = context.destination_url(&request.owner, &request.name);
    let internal_key = context.internal_key_hex();
    context
        .jobs
        .update(
            id,
            JobState::Running,
            format!("Publishing Git history ({} refs)", refs.len()),
        )
        .await;
    push_mirror(&mirror, &destination, &internal_key, &cancellation).await?;

    // Internal import receives acknowledge the durable journal without waiting
    // for read projection. Start one projection after all refs are committed.
    drop(maintenance_guard);
    if let Some(repository) = server
        .repositories
        .get(&(request.owner.clone(), request.name.clone()))
    {
        repository.invalidate().await;
        if let Err(error) = repository
            .schedule_maintenance(server, MaintenanceTrigger::Repair)
            .await
        {
            tracing::warn!(repository_id = %repository.id, error = ?error, "import projection scheduling deferred");
        }
    }
    Ok(())
}

fn actor_key(actor: &Identity) -> String {
    format!("{}\0{}", actor.issuer, actor.subject)
}

fn is_import_git_path(method: &axum::http::Method, path: &str) -> bool {
    let mut segments = path.split('/');
    if segments.next() != Some("") || segments.next() != Some("git") {
        return false;
    }
    if segments.next().is_none_or(str::is_empty) {
        return false;
    }
    let Some(repository) = segments.next() else {
        return false;
    };
    if !repository.ends_with(".git") {
        return false;
    }
    match *method {
        axum::http::Method::POST => {
            segments.next() == Some("git-receive-pack") && segments.next().is_none()
        }
        axum::http::Method::GET => {
            segments.next() == Some("info")
                && segments.next() == Some("refs")
                && segments.next().is_none()
        }
        _ => false,
    }
}

fn encode_path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_authorization_is_narrow() {
        let context = ImportContext::new(test_config(), "127.0.0.1:8788".parse().unwrap());
        let key = context.internal_key_hex();
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/git/team/repo.git/git-receive-pack")
            .header("host", "127.0.0.1:8788")
            .header("x-crab-internal-import", key)
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(context.authorizes_git(&request));
        let request = axum::extract::Request::builder()
            .method("GET")
            .uri("/git/team/repo.git/info/refs/extra/info/refs")
            .header("host", "127.0.0.1:8788")
            .header("x-crab-internal-import", context.internal_key_hex())
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!context.authorizes_git(&request));
        let request = axum::extract::Request::builder()
            .method("GET")
            .uri("/git/team/repo.git/info/refs?service=git-receive-pack")
            .header("host", "127.0.0.1:8788")
            .header("x-crab-internal-import", context.internal_key_hex())
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(context.authorizes_git(&request));
        let request = axum::extract::Request::builder()
            .method("POST")
            .uri("/api/imports/git")
            .header("host", "127.0.0.1:8788")
            .header("x-crab-internal-import", context.internal_key_hex())
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(!context.authorizes_git(&request));
    }

    fn test_config() -> Config {
        // Authorization tests only exercise the derived internal key and host.
        // The import context does not read storage until a job starts.
        toml::from_str(
            r#"
listen = "127.0.0.1:8788"
management_listen = "127.0.0.1:8789"
[storage]
url = "s3://bucket/prefix"
[cells]
data_dir = "/tmp/crab-cells"
local_disk_limit_bytes = 21474836480
peer_advertise = "https://127.0.0.1:8789"
peer_certificate = "/tmp/cert.pem"
peer_private_key = "/tmp/key.pem"
peer_ca = "/tmp/ca.pem"
"#,
        )
        .unwrap()
    }
}
