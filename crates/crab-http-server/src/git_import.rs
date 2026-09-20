use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::Path,
    process::Stdio,
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
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::RwLock,
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};
use uuid::Uuid;

use crate::{
    Config, RepositoryAccess, RepositoryMember, app,
    auth::{Identity, Principal},
    catalog::CatalogStore,
    server::Server,
};

const MAX_ACTIVE_JOBS: usize = 4;
const MAX_RETAINED_JOBS: usize = 256;
const JOB_RETENTION: Duration = Duration::from_secs(60 * 60);
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_TOKEN_BYTES: usize = 512;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const PUSH_REF_BATCH_SIZE: usize = 512;
const MAX_IMPORT_REFS: usize = 100_000;
const MAX_IMPORT_MIRROR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_PUSH_ATTEMPTS: usize = 3;
const PUSH_RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct ImportContext {
    config: Config,
    address: SocketAddr,
    internal_key: [u8; 32],
    allowed_hosts: Arc<[String]>,
    jobs: ImportJobs,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitImportRequest {
    source: String,
    owner: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Clone)]
struct ValidatedImport {
    source: String,
    owner: String,
    name: String,
    description: String,
    token: Option<String>,
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
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "import_busy",
                "Too many imports are running; try again shortly",
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "import_conflict",
                "An import for this repository is already running",
            ),
            Self::Cancelled | Self::Timeout => (
                StatusCode::REQUEST_TIMEOUT,
                "import_cancelled",
                "The import did not finish; retry it to continue",
            ),
            Self::Git(_) => (
                StatusCode::BAD_GATEWAY,
                "git_source_unavailable",
                "The Git source could not be cloned; verify the URL, access, and token",
            ),
            Self::Setup(_) | Self::Catalog(_) | Self::Io(_) => (
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
            job.status.message = "Repository imported".into();
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
                    "The Git source could not be cloned; verify the URL, access, and token".into()
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
    let directory = match server
        .local_staging
        .create(MAX_IMPORT_MIRROR_BYTES, &cancellation)
        .await
    {
        Ok(directory) => directory,
        Err(crate::local_disk::Error::Busy) => return Err(ImportError::Busy),
        Err(crate::local_disk::Error::Cancelled) => return Err(ImportError::Cancelled),
        Err(crate::local_disk::Error::TooLarge) => {
            return Err(ImportError::Git(
                "Git source exceeds the import storage limit".into(),
            ));
        }
        Err(error) => {
            return Err(ImportError::Setup(crate::Error::LocalStaging {
                source: Box::new(error),
            }));
        }
    };
    let mirror = directory.path().join("repository.git");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--mirror", "--quiet", "--"])
        .arg(&request.source)
        .arg(&mirror);
    configure_git(&mut clone, request.token.as_deref(), None);
    run_git(clone, &cancellation, request.token.as_deref()).await?;

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
    crate::cells::initialize_repository(&context.config, record.id)
        .await
        .map_err(ImportError::Setup)?;

    let (document, _) = catalog.load().await.map_err(ImportError::Catalog)?;
    let repositories = crate::server::materialize_catalog(&catalog, document)
        .await
        .map_err(ImportError::Setup)?;
    server.repositories.replace(repositories);

    context
        .jobs
        .update(id, JobState::Running, "Publishing Git history".into())
        .await;
    let destination = context.destination_url(&request.owner, &request.name);
    let batches = refs.len().div_ceil(PUSH_REF_BATCH_SIZE);
    let internal_key = context.internal_key_hex();
    for (index, batch) in refs.chunks(PUSH_REF_BATCH_SIZE).enumerate() {
        context
            .jobs
            .update(
                id,
                JobState::Running,
                format!("Publishing Git history ({}/{batches})", index + 1),
            )
            .await;
        let refspecs = batch
            .iter()
            .map(|name| format!("+{name}:{name}"))
            .collect::<Vec<_>>();
        push_batch(
            &mirror,
            &destination,
            refspecs,
            &internal_key,
            &cancellation,
        )
        .await?;
    }
    Ok(())
}

async fn push_batch(
    mirror: &Path,
    destination: &str,
    refspecs: Vec<String>,
    internal_key: &str,
    cancellation: &CancellationToken,
) -> Result<(), ImportError> {
    let mut attempt = 0;
    loop {
        let mut push = Command::new("git");
        push.current_dir(mirror)
            .args(["push", "--quiet", "--atomic"])
            .arg(destination)
            .args(&refspecs);
        configure_git(&mut push, None, Some(internal_key));
        match run_git(push, cancellation, Some(internal_key)).await {
            Ok(_) => return Ok(()),
            Err(error)
                if matches!(&error, ImportError::Git(_)) && attempt + 1 < MAX_PUSH_ATTEMPTS =>
            {
                attempt += 1;
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_PUSH_ATTEMPTS,
                    error = ?error,
                    "Git import ref batch failed; retrying"
                );
                let delay = PUSH_RETRY_DELAY
                    .checked_mul(attempt as u32)
                    .unwrap_or(PUSH_RETRY_DELAY);
                tokio::select! {
                    () = cancellation.cancelled() => return Err(ImportError::Cancelled),
                    () = tokio::time::sleep(delay) => {}
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn mirror_refs(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<Vec<String>, ImportError> {
    let mut refs = Command::new("git");
    refs.current_dir(path)
        .args(["for-each-ref", "--format=%(refname)"]);
    configure_git(&mut refs, None, None);
    let output = run_git(refs, cancellation, None).await?;
    if output.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(ImportError::Git(
            "Git source advertised too much ref metadata".into(),
        ));
    }
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect())
}

async fn default_branch(
    path: &Path,
    cancellation: &CancellationToken,
) -> Result<String, ImportError> {
    let mut symbolic = Command::new("git");
    symbolic
        .current_dir(path)
        .args(["symbolic-ref", "--short", "HEAD"]);
    configure_git(&mut symbolic, None, None);
    match run_git(symbolic, cancellation, None).await {
        Ok(branch) => {
            let branch = branch.trim();
            if valid_branch(branch) {
                return Ok(branch.to_owned());
            }
        }
        Err(ImportError::Git(_)) => {}
        Err(error) => return Err(error),
    }

    let mut refs = Command::new("git");
    refs.current_dir(path)
        .args(["for-each-ref", "--format=%(refname:strip=2)", "refs/heads"]);
    configure_git(&mut refs, None, None);
    let refs = run_git(refs, cancellation, None).await?;
    let mut branches = refs.lines().filter(|branch| valid_branch(branch));
    branches
        .find(|branch| *branch == "main")
        .or_else(|| {
            let mut fallback = refs.lines().filter(|branch| valid_branch(branch));
            fallback.find(|branch| *branch == "master")
        })
        .or_else(|| refs.lines().find(|branch| valid_branch(branch)))
        .map(str::to_owned)
        .ok_or(ImportError::Git("Git source has no branch".into()))
}

fn configure_git(command: &mut Command, source_token: Option<&str>, internal_key: Option<&str>) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", empty_git_config())
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "http.followRedirects")
        .env("GIT_CONFIG_VALUE_1", "false");
    if let Some(token) = source_token {
        command
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_2", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_2",
                format!("Authorization: Bearer {token}"),
            );
    }
    if let Some(key) = internal_key {
        command
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_2", "http.extraHeader")
            .env(
                "GIT_CONFIG_VALUE_2",
                format!("X-Crab-Internal-Import: {key}"),
            );
    }
}

fn empty_git_config() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

async fn run_git(
    mut command: Command,
    cancellation: &CancellationToken,
    secret: Option<&str>,
) -> Result<String, ImportError> {
    command
        .kill_on_drop(true)
        .env("GIT_TRACE", "0")
        .env("GIT_TRACE_CURL", "0")
        .env("GIT_CURL_VERBOSE", "0")
        .env("GIT_TRACE_PACKET", "0")
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(ImportError::Io)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ImportError::Io(io::Error::other("git stdout was not piped for import")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ImportError::Io(io::Error::other("git stderr was not piped for import")))?;
    let (status, stdout, stderr) = tokio::select! {
        result = tokio::time::timeout(GIT_COMMAND_TIMEOUT, async {
            let (stdout, stderr) = tokio::try_join!(
                read_git_output(stdout),
                read_git_output(stderr),
            )?;
            let status = child.wait().await.map_err(GitOutputError::Io)?;
            Ok::<_, GitOutputError>((status, stdout, stderr))
        }) => {
            match result {
                Ok(Ok(output)) => output,
                Ok(Err(GitOutputError::Limit)) => {
                    return Err(ImportError::Git(
                        "Git command output exceeded the import limit".into(),
                    ));
                }
                Ok(Err(GitOutputError::Io(error))) => return Err(ImportError::Io(error)),
                Err(_) => return Err(ImportError::Timeout),
            }
        }
        () = cancellation.cancelled() => return Err(ImportError::Cancelled),
    };
    if !status.success() {
        let mut detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
            detail = detail.replace(secret, "[redacted]");
        }
        if detail.is_empty() {
            detail = "git exited unsuccessfully".into();
        }
        detail.truncate(512);
        return Err(ImportError::Git(detail));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[derive(Debug)]
enum GitOutputError {
    Io(io::Error),
    Limit,
}

async fn read_git_output<R>(mut reader: R) -> Result<Vec<u8>, GitOutputError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await.map_err(GitOutputError::Io)?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_GIT_OUTPUT_BYTES {
            return Err(GitOutputError::Limit);
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

fn validate_request(
    request: GitImportRequest,
    allowed_hosts: &[String],
) -> Result<ValidatedImport, ImportError> {
    let source = normalize_source(&request.source, allowed_hosts)?;
    let owner = identifier(request.owner, "owner")?;
    let name = identifier(request.name, "name")?;
    let description = request.description.unwrap_or_default().trim().to_owned();
    if description.chars().count() > MAX_DESCRIPTION_CHARS
        || description.chars().any(char::is_control)
    {
        return Err(ImportError::Invalid(
            "description must be at most 1,000 characters without controls",
        ));
    }
    let token = request
        .token
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    if token.as_deref().is_some_and(|token| {
        token.len() > MAX_TOKEN_BYTES || token.bytes().any(|byte| !byte.is_ascii_graphic())
    }) {
        return Err(ImportError::Invalid("the Git source token is invalid"));
    }
    if token.is_some() && !source.starts_with("https://") {
        return Err(ImportError::Invalid(
            "source tokens require an HTTPS Git URL",
        ));
    }
    Ok(ValidatedImport {
        source,
        owner,
        name,
        description,
        token,
    })
}

fn normalize_source(value: &str, allowed_hosts: &[String]) -> Result<String, ImportError> {
    let value = value.trim();
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    if let Some(path) = value
        .strip_prefix("github.com:")
        .or_else(|| value.strip_prefix("git@github.com:"))
    {
        return normalize_github_path(path, allowed_hosts);
    }
    if !value.contains("://") && !value.starts_with("git@") {
        let mut segments = value.split('/');
        let owner = segments.next().unwrap_or_default();
        let name = segments.next().unwrap_or_default();
        if segments.next().is_none() && valid_identifier(owner) && valid_identifier(name) {
            return normalize_github_path(value, allowed_hosts);
        }
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    if let Some(rest) = value.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return normalize_ssh_source(host, path, allowed_hosts);
    }
    let url = Url::parse(value)
        .map_err(|_| ImportError::Invalid("source must be an allowed HTTPS or SSH clone URL"))?;
    if !matches!(url.scheme(), "https" | "ssh")
        || url.host_str().is_none()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ImportError::Invalid(
            "source must be an allowed HTTPS or SSH clone URL",
        ));
    }
    let host = url
        .host_str()
        .ok_or(ImportError::Invalid("source host is required"))?;
    if has_dot_path_segment(value) {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    let host = normalize_host(host)?;
    require_allowed_host(&host, allowed_hosts)?;
    if url.scheme() == "https" && !url.username().is_empty() {
        return Err(ImportError::Invalid(
            "HTTPS source URLs must not contain a username",
        ));
    }
    if url.scheme() == "ssh" && !url.username().is_empty() && url.username() != "git" {
        return Err(ImportError::Invalid(
            "SSH source URLs must use the git user",
        ));
    }
    let path = normalize_source_path(url.path())?;
    if host == "github.com" {
        if url.scheme() == "ssh" && url.port().is_some() {
            return Err(ImportError::Invalid(
                "GitHub SSH URLs must use the default SSH port",
            ));
        }
        let port = url
            .port()
            .map_or_else(String::new, |port| format!(":{port}"));
        return Ok(format!("https://github.com{port}/{path}"));
    }
    let scheme = url.scheme();
    let user = if scheme == "ssh" { "git@" } else { "" };
    let port = url
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    let host = format_url_host(&host);
    Ok(format!("{scheme}://{user}{host}{port}/{path}"))
}

fn normalize_github_path(path: &str, allowed_hosts: &[String]) -> Result<String, ImportError> {
    require_allowed_host("github.com", allowed_hosts)?;
    let path = normalize_source_path(path)?;
    let mut segments = path.trim_end_matches(".git").split('/');
    let owner = segments.next().unwrap_or_default();
    let name = segments.next().unwrap_or_default();
    if segments.next().is_some() || !valid_identifier(owner) || !valid_identifier(name) {
        return Err(ImportError::Invalid(
            "GitHub shorthand must be owner/repository",
        ));
    }
    Ok(format!("https://github.com/{owner}/{name}.git"))
}

fn normalize_ssh_source(
    host: &str,
    path: &str,
    allowed_hosts: &[String],
) -> Result<String, ImportError> {
    let host = normalize_host(host)?;
    require_allowed_host(&host, allowed_hosts)?;
    let path = normalize_source_path(path)?;
    if host == "github.com" {
        return normalize_github_path(&path, allowed_hosts);
    }
    Ok(format!("ssh://git@{}/{path}", format_url_host(&host)))
}

fn format_url_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn has_dot_path_segment(value: &str) -> bool {
    let Some(scheme_end) = value.find("://") else {
        return false;
    };
    let authority_start = scheme_end + 3;
    let Some(path_offset) = value[authority_start..].find('/') else {
        return false;
    };
    let path_start = authority_start + path_offset;
    let path_end = value[path_start..]
        .find(['?', '#'])
        .map_or(value.len(), |offset| path_start + offset);
    value[path_start..path_end]
        .split('/')
        .any(is_dot_path_segment)
}

fn is_dot_path_segment(segment: &str) -> bool {
    let mut decoded = String::with_capacity(segment.len());
    let bytes = segment.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            decoded.push(char::from(high * 16 + low));
            index += 3;
        } else {
            decoded.push(char::from(bytes[index]));
            index += 1;
        }
    }
    matches!(decoded.as_str(), "." | "..")
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn normalize_host(value: &str) -> Result<String, ImportError> {
    let host = Host::parse(value).map_err(|_| ImportError::Invalid("source host is invalid"))?;
    let normalized = match host {
        Host::Domain(value) => value.to_ascii_lowercase(),
        Host::Ipv4(value) => value.to_string(),
        Host::Ipv6(value) => value.to_string(),
    };
    if normalized.is_empty() || normalized.len() > 253 {
        return Err(ImportError::Invalid("source host is invalid"));
    }
    Ok(normalized)
}

fn require_allowed_host(host: &str, allowed_hosts: &[String]) -> Result<(), ImportError> {
    if allowed_hosts.iter().any(|allowed| allowed == host) {
        Ok(())
    } else {
        Err(ImportError::Invalid(
            "source host is not enabled by the server import allowlist",
        ))
    }
}

fn normalize_source_path(value: &str) -> Result<String, ImportError> {
    let path = value.trim_matches('/');
    if path.is_empty()
        || path.len() > 4_096
        || path.chars().any(char::is_control)
        || path.bytes().any(|byte| matches!(byte, b'?' | b'#'))
    {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty()
        || path
            .split('/')
            .any(|segment| segment.is_empty() || is_dot_path_segment(segment))
    {
        return Err(ImportError::Invalid("source repository path is invalid"));
    }
    Ok(format!("{path}.git"))
}

#[cfg(test)]
fn normalize_github_source(value: &str) -> Result<String, ImportError> {
    normalize_source(value, &["github.com".to_owned()])
}

fn identifier(value: String, field: &'static str) -> Result<String, ImportError> {
    let value = value.trim().to_owned();
    if !valid_identifier(&value)
        || (field == "owner" && matches!(value.as_str(), "api" | "assets" | "auth" | "git"))
    {
        return Err(ImportError::Invalid(
            "owner and name must be URL-safe identifiers",
        ));
    }
    Ok(value)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && !matches!(value, "." | "..")
}

fn valid_branch(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with("refs/")
        && crab_git::validate_push_refname(&format!("refs/heads/{value}")).is_ok()
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
    fn normalizes_github_locators_without_credentials() {
        for value in [
            "denoland/celld",
            "https://github.com/denoland/celld.git",
            "git@github.com:denoland/celld.git",
            "ssh://git@github.com/denoland/celld",
        ] {
            assert_eq!(
                normalize_github_source(value).unwrap(),
                "https://github.com/denoland/celld.git"
            );
        }
    }

    #[test]
    fn rejects_source_credentials_and_non_github_hosts() {
        for value in [
            "https://user:secret@github.com/team/repo",
            "https://github.com/team/repo?token=secret",
            "https://gitlab.com/team/repo",
            "team/repo/extra",
        ] {
            assert!(normalize_github_source(value).is_err(), "{value}");
        }
    }

    #[test]
    fn accepts_allowlisted_git_hosts_and_rejects_untrusted_transports() {
        let allowed = ["github.com".to_owned(), "gitlab.com".to_owned()];
        assert_eq!(
            normalize_source("https://gitlab.com/group/repo", &allowed).unwrap(),
            "https://gitlab.com/group/repo.git"
        );
        assert_eq!(
            normalize_source("https://gitlab.com/group/subgroup/repo.git", &allowed,).unwrap(),
            "https://gitlab.com/group/subgroup/repo.git"
        );
        assert_eq!(
            normalize_source("git@gitlab.com:group/repo.git", &allowed).unwrap(),
            "ssh://git@gitlab.com/group/repo.git"
        );
        let ipv6 = ["2001:db8::7".to_owned()];
        assert_eq!(
            normalize_source("ssh://git@[2001:db8::7]:2222/group/repo", &ipv6).unwrap(),
            "ssh://git@[2001:db8::7]:2222/group/repo.git"
        );
        for value in [
            "https://gitlab.com/group/repo?token=secret",
            "https://gitlab.com/group/../repo",
            "https://gitlab.com/group/%2e%2e/repo",
            "git@gitlab.com:group/repo?token=secret",
            "git://gitlab.com/group/repo",
            "https://example.com/group/repo",
        ] {
            assert!(normalize_source(value, &allowed).is_err(), "{value}");
        }
    }

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
