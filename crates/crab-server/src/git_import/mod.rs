mod authority;
mod git;
mod jobs;
mod source;
mod workflow;
use authority::{ForkSource, ImportAccount, OrganizationDestination};
use source::{GitImportRequest, ValidatedImport, validate_request};
use workflow::run_import;

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Extension, Json, Router,
    extract::{Path as AxumPath, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::{
    Config, app, auth::Principal, cells::organization::properties::PropertyValue,
    repository::Repository, server::Server,
};

#[derive(Clone)]
pub(crate) struct ImportContext {
    address: SocketAddr,
    allowed_hosts: Arc<[String]>,
    admission: jobs::Admission,
    importing: Arc<Mutex<HashMap<blake3::Hash, InternalGit>>>,
}

impl ImportContext {
    pub(crate) fn new(config: &Config, address: SocketAddr) -> Self {
        let allowed_hosts = config
            .import
            .allowed_hosts
            .iter()
            .map(|host| host.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .into();
        Self::with_hosts(address, allowed_hosts)
    }

    #[cfg(test)]
    pub(crate) fn for_test(address: SocketAddr) -> Self {
        Self::with_hosts(
            address,
            vec!["127.0.0.1".into(), "github.com".into()].into(),
        )
    }

    fn with_hosts(address: SocketAddr, allowed_hosts: Arc<[String]>) -> Self {
        Self {
            address,
            allowed_hosts,
            admission: jobs::Admission::default(),
            importing: Arc::default(),
        }
    }

    fn begin_repository(
        &self,
        repository: Arc<Repository>,
        writer: Arc<jobs::Writer>,
        snapshot: jobs::Snapshot,
        organization: Option<OrganizationDestination>,
        fork: Option<Arc<ForkSource>>,
    ) -> ImportGuard {
        self.register(InternalGit::Receive(Arc::new(ReceiveGrant {
            route: repository_route(&repository),
            repository,
            writer,
            snapshot,
            organization,
            fork,
        })))
    }

    fn register(&self, grant: InternalGit) -> ImportGuard {
        let key = blake3::Hash::from_bytes(rand::random());
        self.importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, grant);
        ImportGuard {
            importing: Arc::clone(&self.importing),
            key,
        }
    }

    pub(crate) fn authorizes_git(&self, request: &axum::extract::Request) -> Option<InternalGit> {
        let key = blake3::Hash::from_hex(
            request
                .headers()
                .get("x-crab-internal-import")?
                .to_str()
                .ok()?,
        )
        .ok()?;
        if !self.host_matches(
            request
                .headers()
                .get("host")
                .and_then(|value| value.to_str().ok()),
        ) {
            return None;
        }
        let grant = self
            .importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)?
            .clone();
        grant
            .accepts(request.method(), request.uri())
            .then_some(grant)
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

#[derive(Clone)]
pub(crate) enum InternalGit {
    Receive(Arc<ReceiveGrant>),
    Fetch(Arc<FetchGrant>),
}

impl InternalGit {
    fn accepts(&self, method: &axum::http::Method, uri: &axum::http::Uri) -> bool {
        match self {
            Self::Receive(grant) => grant.accepts(method, uri),
            Self::Fetch(grant) => matches!(
                (method, uri.path().strip_prefix(&grant.route), uri.query()),
                (&axum::http::Method::POST, Some("/git-upload-pack"), None)
                    | (
                        &axum::http::Method::GET,
                        Some("/info/refs"),
                        Some("service=git-upload-pack")
                    )
            ),
        }
    }

    pub(crate) async fn principal(self, server: &Server) -> Principal {
        match self {
            Self::Receive(grant) => Principal::import(grant),
            Self::Fetch(grant) => grant
                .principal(server)
                .await
                .unwrap_or_else(|_| Principal::anonymous()),
        }
    }
}

pub(crate) struct FetchGrant {
    route: String,
    writer: Arc<jobs::Writer>,
    fork: Arc<ForkSource>,
    destination: Option<OrganizationDestination>,
}

impl FetchGrant {
    async fn principal(&self, server: &Server) -> Result<Principal, ImportError> {
        self.writer.verify().await?;
        if let Some(owner) = self.destination {
            owner.check(server).await?;
        }
        let (source, principal) = self.fork.current(server).await?;
        if repository_route(&source) != self.route {
            return Err(ImportError::NotFound);
        }
        Ok(principal.scoped_to_repository(Some(source.id)))
    }
}

fn repository_route(repository: &Repository) -> String {
    format!(
        "/git/{}/{}.git",
        encode_path_segment(&repository.config.owner),
        encode_path_segment(&repository.config.name)
    )
}

pub(crate) struct ReceiveGrant {
    pub(crate) repository: Arc<Repository>,
    writer: Arc<jobs::Writer>,
    snapshot: jobs::Snapshot,
    organization: Option<OrganizationDestination>,
    fork: Option<Arc<ForkSource>>,
    route: String,
}

impl ReceiveGrant {
    fn accepts(&self, method: &axum::http::Method, uri: &axum::http::Uri) -> bool {
        let tail = uri.path().strip_prefix(&self.route);
        matches!(
            (method, tail, uri.query()),
            (&axum::http::Method::POST, Some("/git-receive-pack"), None)
                | (
                    &axum::http::Method::GET,
                    Some("/info/refs"),
                    Some("service=git-receive-pack")
                )
        )
    }

    pub(crate) async fn active(&self, server: &Server) -> bool {
        self.current_owner(server).await.is_some()
    }

    pub(crate) async fn current_owner(&self, server: &Server) -> Option<bool> {
        let result = async {
            self.writer.verify().await?;
            let owner = match self.organization {
                Some(destination) => destination.check(server).await?,
                None => false,
            };
            if let Some(fork) = &self.fork {
                fork.current(server).await?;
            }
            Ok::<_, ImportError>(owner)
        }
        .await;
        match result {
            Ok(owner) => Some(owner),
            Err(error) => {
                tracing::warn!(error = ?error, "Git import authorization expired or could not be verified");
                None
            }
        }
    }

    pub(crate) async fn fork_source(
        &self,
        server: &Server,
    ) -> Result<Option<Arc<Repository>>, crate::git::receive::ReceiveError> {
        use crate::git::receive::ReceiveError;
        if self.repository.forked_from.is_none() {
            return Ok(None);
        }
        if !self.active(server).await {
            return Err(ReceiveError::Forbidden);
        }
        let source = self
            .repository
            .fork_source(server)
            .ok_or(ReceiveError::NotFound)?;
        Ok(Some(source))
    }

    pub(crate) async fn policy_grant(
        &self,
        server: &Server,
    ) -> crate::app::Result<crate::cells::organization::teams::repository_access::ActorTeams> {
        match self.organization {
            Some(owner) => owner.policy_grant(server).await,
            None => match &self.fork {
                Some(fork) => fork
                    .policy_grant(server, true)
                    .await
                    .map_err(|_| app::Error::Forbidden),
                None => Ok(Default::default()),
            },
        }
    }

    pub(crate) fn accepts_updates(
        &self,
        head: &str,
        refs: &BTreeMap<String, String>,
        updates: &[crab_git::receive_plan::RefUpdate],
    ) -> bool {
        // A restarted job must never overwrite refs or publish a different snapshot.
        refs.is_empty()
            && updates
                .iter()
                .map(|update| {
                    let target = update.new.filter(|_| update.old.is_none())?;
                    Some((update.name.clone(), target.to_string()))
                })
                .collect::<Option<BTreeMap<_, _>>>()
                .is_some_and(|new_refs| self.snapshot.matches(head, &new_refs))
    }
}

struct ImportGuard {
    importing: Arc<Mutex<HashMap<blake3::Hash, InternalGit>>>,
    key: blake3::Hash,
}

impl Drop for ImportGuard {
    fn drop(&mut self) {
        self.importing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

#[derive(Debug, thiserror::Error)]
enum ImportError {
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    #[error("the import service is unavailable")]
    Unavailable,
    #[error("the import job was not found")]
    NotFound,
    #[error("organization ruleset restricts an imported ref")]
    Ruleset,
    #[error("the organization or repository does not allow private forks")]
    ForkingDisabled,
    #[error("the organization restricts creation at this visibility")]
    CreationRestricted,
    #[error("the source organization does not allow forks into other organizations")]
    ExternalForkingDisabled,
    #[error("the organization import authority is unavailable")]
    Organization(#[source] Box<app::Error>),
    #[error("the import queue is busy")]
    Busy,
    #[error("import staging failed")]
    Staging(#[source] crate::local_disk::Error),
    #[error("an import for this repository is already running")]
    Conflict,
    #[error("the import request ID was already used for a different import")]
    SubmissionConflict,
    #[error("the Git source no longer matches the pinned snapshot")]
    SourceChanged,
    #[error("the import destination no longer matches the reserved repository")]
    DestinationChanged,
    #[error("the import worker lost its durable lease")]
    LeaseLost,
    #[error("durable Git import status storage failed")]
    JobStorage(#[from] crab_storage::StorageError),
    #[error("durable Git import status encoding failed")]
    JobEncoding(#[from] serde_json::Error),
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

impl ImportError {
    fn organization(source: app::Error) -> Self {
        match source {
            app::Error::NotFound => Self::NotFound,
            source => Self::Organization(Box::new(source)),
        }
    }
}

impl IntoResponse for ImportError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Invalid(message) => (StatusCode::BAD_REQUEST, "invalid_request", message),
            Self::Unavailable | Self::Organization(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "import_unavailable",
                "Git import is unavailable on this server",
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", "Import job not found"),
            Self::Ruleset => (
                StatusCode::FORBIDDEN,
                "repository_ruleset",
                "Organization rules restrict the imported refs",
            ),
            Self::CreationRestricted => (
                StatusCode::FORBIDDEN,
                "repository_creation_restricted",
                "The organization restricts repository creation at this visibility",
            ),
            Self::ForkingDisabled => (
                StatusCode::FORBIDDEN,
                "repository_forking_restricted",
                "The organization or source repository does not allow private forks",
            ),
            Self::ExternalForkingDisabled => (
                StatusCode::FORBIDDEN,
                "external_forking_restricted",
                "The source organization does not allow forks into other organizations",
            ),
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
            Self::SubmissionConflict => (
                StatusCode::CONFLICT,
                "import_submission_conflict",
                "This import request ID belongs to a different submission",
            ),
            Self::SourceChanged | Self::DestinationChanged | Self::LeaseLost => (
                StatusCode::CONFLICT,
                "import_conflict",
                "The import no longer matches the current job state",
            ),
            Self::JobStorage(_) | Self::JobEncoding(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "import_status_unavailable",
                "Import status is temporarily unavailable. Retry the same request.",
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
            Self::Invalid(_)
                | Self::NotFound
                | Self::Busy
                | Self::Conflict
                | Self::Ruleset
                | Self::CreationRestricted
                | Self::ForkingDisabled
                | Self::ExternalForkingDisabled
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
        .route(
            "/api/repos/{owner}/{name}/forks",
            post(start_fork).layer(axum::extract::DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/api/imports/git/{id}", get(status))
        .route("/api/imports/git/{id}/resume", post(resume))
        .route(
            "/api/organizations/{organization}/repositories/imports/git",
            post(start).layer(axum::extract::DefaultBodyLimit::max(128 * 1024)),
        )
        .route(
            "/api/organizations/{organization}/repositories/imports/git/{id}",
            get(status),
        )
        .route(
            "/api/organizations/{organization}/repositories/imports/git/{id}/resume",
            post(resume),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
struct ImportPath {
    organization: Option<Uuid>,
    id: Option<Uuid>,
}

async fn start(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    AxumPath(path): AxumPath<ImportPath>,
    body: Result<Json<GitImportRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<jobs::Status>), ImportError> {
    let context = server.git_import.as_ref().ok_or(ImportError::Unavailable)?;
    let account = ImportAccount::resolve(&server, &principal, path.organization).await?;
    let Json(request) = body.map_err(|_| ImportError::Invalid("invalid import request"))?;
    let id = request.request_id.unwrap_or_else(Uuid::now_v7);
    let request = validate_request(request, path.organization, &context.allowed_hosts)?;
    submit(&server, context, account, id, request).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkRequest {
    request_id: Uuid,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default_branch_only: bool,
    #[serde(default)]
    destination: ForkDestination,
    organization: Option<Uuid>,
    #[serde(default)]
    properties: BTreeMap<Uuid, PropertyValue>,
}

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ForkDestination {
    #[default]
    Organization,
    Personal,
}

async fn start_fork(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    AxumPath(key): AxumPath<(String, String)>,
    Json(input): Json<ForkRequest>,
) -> Result<(StatusCode, Json<jobs::Status>), ImportError> {
    let context = server.git_import.as_ref().ok_or(ImportError::Unavailable)?;
    let source = app::repository(&server, &principal, &key).map_err(ImportError::organization)?;
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let (document, _) = catalog
        .load_snapshot()
        .await
        .map_err(ImportError::Catalog)?;
    let record = document
        .repository_by_id(source.id)
        .filter(|record| record.forked_from.is_none())
        .ok_or(ImportError::NotFound)?;
    let source_organization = record.organization_id();
    let visibility = record.visibility.value;
    if source_organization.is_none() && !visibility.is_public() {
        return Err(ImportError::NotFound);
    }
    let personal = input.destination == ForkDestination::Personal;
    if personal && input.organization.is_some() {
        return Err(ImportError::Invalid("Choose one fork destination"));
    }
    if personal && !input.properties.is_empty() {
        return Err(ImportError::Invalid(
            "Repository properties require an organization destination",
        ));
    }
    let organization = if personal {
        None
    } else {
        Some(
            input
                .organization
                .or(source_organization)
                .ok_or(ImportError::Invalid("Choose a destination organization"))?,
        )
    };
    let account = ImportAccount::resolve(&server, &principal, organization).await?;
    let name = source::identifier(input.name, "name")?;
    if organization.is_some()
        && organization == source_organization
        && name.eq_ignore_ascii_case(&source.config.name)
    {
        return Err(ImportError::Invalid("Choose a different name for the fork"));
    }
    let request = ValidatedImport {
        source: format!("{}/{}", source.config.owner, source.config.name),
        fork_source: Some(source.id),
        default_branch_only: input.default_branch_only,
        owner: match organization {
            Some(id) => crate::catalog::organization_namespace(id),
            None => account.personal_owner(),
        },
        name,
        description: source::description(&input.description)?,
        visibility,
        properties: input.properties,
        token: None,
    };
    submit(&server, context, account, input.request_id, request).await
}

async fn submit(
    server: &Arc<Server>,
    context: &ImportContext,
    account: ImportAccount,
    id: Uuid,
    request: ValidatedImport,
) -> Result<(StatusCode, Json<jobs::Status>), ImportError> {
    if id.is_nil() {
        return Err(ImportError::Invalid("the import request ID is invalid"));
    }
    let account = account.for_request(&request);
    let organization_owner = account.check(server).await?;
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    account.namespace(&request.owner, request.visibility)?;
    let jobs = account.jobs(catalog.root());
    if let Some(status) = jobs.replay(id, &request).await? {
        account.check_viewer(server).await?;
        return Ok((StatusCode::ACCEPTED, Json(status)));
    }
    if let Some(owner) = account.organization {
        owner
            .validate_properties(server, &request.properties)
            .await?;
    }
    account
        .fork_source(server, &catalog, request.fork_source, request.visibility)
        .await?;
    let slot = context.admission.admit(&request)?;
    let Some(writer) = jobs
        .create(id, &request, account.member_creator(organization_owner))
        .await?
    else {
        let status = jobs
            .replay(id, &request)
            .await?
            .ok_or(ImportError::Unavailable)?;
        account.check_viewer(server).await?;
        return Ok((StatusCode::ACCEPTED, Json(status)));
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(
            spawn_import(
                server.clone(),
                context.clone(),
                writer,
                request,
                account,
                slot,
            )
            .await,
        ),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeRequest {
    token: Option<String>,
}

async fn resume(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    AxumPath(path): AxumPath<ImportPath>,
    body: Result<Json<ResumeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<jobs::Status>), ImportError> {
    let context = server.git_import.as_ref().ok_or(ImportError::Unavailable)?;
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let account = ImportAccount::resolve(&server, &principal, path.organization).await?;
    let id = path.id.ok_or(ImportError::NotFound)?;
    let Json(ResumeRequest { token }) =
        body.map_err(|_| ImportError::Invalid("invalid resume request"))?;
    let jobs = account.jobs(catalog.root());
    let (status, request) = jobs
        .resume_request(id, token, &context.allowed_hosts)
        .await?;
    account.check_viewer(&server).await?;
    require_job_source(&server, &account, &status).await?;
    let Some(request) = request else {
        return Ok((StatusCode::ACCEPTED, Json(status)));
    };
    let account = account.for_request(&request);
    account.check(&server).await?;
    account.namespace(&request.owner, request.visibility)?;
    account
        .fork_source(&server, &catalog, request.fork_source, request.visibility)
        .await?;
    let slot = context.admission.admit(&request)?;
    let status = match jobs.claim(id, &request).await? {
        Some(writer) => {
            spawn_import(
                server.clone(),
                context.clone(),
                writer,
                request,
                account,
                slot,
            )
            .await
        }
        None => jobs.status(id).await?,
    };
    Ok((StatusCode::ACCEPTED, Json(status)))
}

async fn spawn_import(
    server: Arc<Server>,
    context: ImportContext,
    writer: jobs::Writer,
    request: ValidatedImport,
    account: ImportAccount,
    slot: jobs::Slot,
) -> jobs::Status {
    let status = writer.queued().await;
    let worker_server = Arc::clone(&server);
    server.receives.spawn(async move {
        let _slot = slot;
        run_import(
            worker_server,
            context,
            Arc::new(writer),
            request,
            account,
            status.id,
        )
        .await;
    });
    status
}

async fn status(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    AxumPath(path): AxumPath<ImportPath>,
) -> Result<Json<jobs::Status>, ImportError> {
    let catalog = server.catalog().ok_or(ImportError::Unavailable)?;
    let account = ImportAccount::resolve(&server, &principal, path.organization).await?;
    let id = path.id.ok_or(ImportError::NotFound)?;
    let status = account.jobs(catalog.root()).status(id).await?;
    account.check_viewer(&server).await?;
    require_job_source(&server, &account, &status).await?;
    app::account(&principal).map_err(|_| ImportError::NotFound)?;
    Ok(Json(status))
}

async fn require_job_source(
    server: &Server,
    account: &ImportAccount,
    status: &jobs::Status,
) -> Result<(), ImportError> {
    let (Some(_), Some(source)) = (account.organization, status.fork_source) else {
        return Ok(());
    };
    let upstream = server
        .repositories
        .by_id(source)
        .ok_or(ImportError::NotFound)?;
    // Shared destination job status contains the upstream repository's address.
    let mut principal = account.principal.clone();
    if status.visibility.is_public() {
        principal.allow_public_reads();
    }
    app::catalog_membership(server, &principal, &upstream)
        .await
        .map_err(ImportError::organization)?;
    Ok(())
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
mod tests;
