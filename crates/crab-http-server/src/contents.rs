use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use base64::Engine;
use gix_hash::ObjectId;
use gix_object::{Kind, WriteTo, bstr::BString, tree};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    app,
    auth::{Identity, Principal},
    receive::{self, ReceiveError},
    server::Server,
};

const MAX_PATH_BYTES: usize = 1024;
const MAX_CONTENT_BYTES: usize = 900 * 1024;
const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024;
const MAX_UPLOAD_FILES: usize = 100;
const MAX_MESSAGE_CHARS: usize = 256;

pub(crate) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/contents",
            post(create)
                .patch(update)
                .delete(remove)
                .layer(axum::extract::DefaultBodyLimit::max(6 * 1024 * 1024)),
        )
        .route(
            "/api/repos/{owner}/{name}/uploads",
            post(upload).layer(axum::extract::DefaultBodyLimit::max(6 * 1024 * 1024)),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    branch: String,
    expected_head: String,
    #[serde(default)]
    new_branch: Option<String>,
    path_hex: String,
    content: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateInput {
    branch: String,
    expected_head: String,
    #[serde(default)]
    new_branch: Option<String>,
    expected_blob: String,
    path_hex: String,
    content: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteInput {
    branch: String,
    expected_head: String,
    #[serde(default)]
    new_branch: Option<String>,
    expected_blob: String,
    path_hex: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadInput {
    branch: String,
    expected_head: String,
    #[serde(default)]
    new_branch: Option<String>,
    files: Vec<UploadFileInput>,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadFileInput {
    path_hex: String,
    content_base64: String,
}

enum ChangeInput {
    Create(CreateInput),
    Update(UpdateInput),
    Delete(DeleteInput),
}

impl ChangeInput {
    fn branch(&self) -> &str {
        match self {
            Self::Create(input) => &input.branch,
            Self::Update(input) => &input.branch,
            Self::Delete(input) => &input.branch,
        }
    }

    fn expected_head(&self) -> &str {
        match self {
            Self::Create(input) => &input.expected_head,
            Self::Update(input) => &input.expected_head,
            Self::Delete(input) => &input.expected_head,
        }
    }

    fn new_branch(&self) -> Option<&str> {
        match self {
            Self::Create(input) => input.new_branch.as_deref(),
            Self::Update(input) => input.new_branch.as_deref(),
            Self::Delete(input) => input.new_branch.as_deref(),
        }
    }

    fn expected_blob(&self) -> Option<&str> {
        match self {
            Self::Create(_) => None,
            Self::Update(input) => Some(&input.expected_blob),
            Self::Delete(input) => Some(&input.expected_blob),
        }
    }

    fn path_hex(&self) -> &str {
        match self {
            Self::Create(input) => &input.path_hex,
            Self::Update(input) => &input.path_hex,
            Self::Delete(input) => &input.path_hex,
        }
    }

    fn content(&self) -> Option<&str> {
        match self {
            Self::Create(input) => Some(&input.content),
            Self::Update(input) => Some(&input.content),
            Self::Delete(_) => None,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::Create(input) => &input.message,
            Self::Update(input) => &input.message,
            Self::Delete(input) => &input.message,
        }
    }

    const fn status(&self) -> StatusCode {
        match self {
            Self::Create(_) => StatusCode::CREATED,
            Self::Update(_) | Self::Delete(_) => StatusCode::OK,
        }
    }
}

#[derive(Serialize)]
struct ChangeOutput {
    branch: String,
    commit: String,
    path_hex: String,
}

#[derive(Serialize)]
struct UploadOutput {
    branch: String,
    commit: String,
    paths_hex: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("{0}")]
    Input(&'static str),
    #[error("Write access is required to change files")]
    Permission,
    #[error("The branch changed; reload before committing")]
    Conflict,
    #[error("A branch with this name already exists or conflicts with another branch")]
    BranchExists,
    #[error("A file or directory already exists at this path")]
    Exists,
    #[error("The file no longer exists")]
    Missing,
    #[error("The file changed; reload before committing")]
    FileChanged,
    #[error("Only regular files can be changed in the browser")]
    Unsupported,
    #[error("The file content is unchanged")]
    Unchanged,
    #[error("A path component is not a directory")]
    NotDirectory,
    #[error("Repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object decoding failed")]
    Decode(#[from] gix_object::decode::Error),
    #[error("Git object encoding failed")]
    Io(#[from] std::io::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("Repository publication failed")]
    Receive(#[source] Box<ReceiveError>),
    #[error("Repository request failed")]
    App(#[from] app::Error),
    #[error("Repository service failed")]
    Service(#[from] crate::Error),
    #[error("Clock failed")]
    Clock(#[from] std::time::SystemTimeError),
    #[error("Invalid request body")]
    Body(#[from] JsonRejection),
}

impl From<ReceiveError> for Error {
    fn from(error: ReceiveError) -> Self {
        Self::Receive(Box::new(error))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Input(message) => (StatusCode::BAD_REQUEST, "invalid_request", *message),
            Self::App(app::Error::NotFound) => (
                StatusCode::NOT_FOUND,
                "repository_not_found",
                "Repository not found",
            ),
            Self::App(app::Error::Invalid(message)) => {
                (StatusCode::BAD_REQUEST, "invalid_request", *message)
            }
            Self::Permission => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to change files",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Forbidden) => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "Write access is required to change files",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Protected) => (
                StatusCode::FORBIDDEN,
                "protected_branch",
                "Protected branch requires a pull request",
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "conflict",
                "The branch changed; reload before committing",
            ),
            Self::BranchExists => (
                StatusCode::CONFLICT,
                "branch_exists",
                "A branch with this name already exists or conflicts with another branch",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Request(_)) => (
                StatusCode::CONFLICT,
                "conflict",
                "The branch changed; reload before committing",
            ),
            Self::Receive(error)
                if matches!(
                    error.as_ref(),
                    ReceiveError::Graph(
                        crab_git::receive_plan::ReceivePlanError::Stale { .. }
                            | crab_git::receive_plan::ReceivePlanError::NonFastForward { .. },
                    ) | ReceiveError::Write(crab_write::WriteError::RefChanged { .. })
                ) =>
            {
                (
                    StatusCode::CONFLICT,
                    "conflict",
                    "The branch changed; reload before committing",
                )
            }
            Self::Receive(error)
                if matches!(
                    error.as_ref(),
                    ReceiveError::Graph(crab_git::receive_plan::ReceivePlanError::Namespace(_))
                        | ReceiveError::Write(crab_write::WriteError::Namespace(_))
                ) =>
            {
                (
                    StatusCode::CONFLICT,
                    "branch_exists",
                    "A branch with this name already exists or conflicts with another branch",
                )
            }
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Graph(_)) => (
                StatusCode::BAD_REQUEST,
                "invalid_path",
                "The file path cannot be committed",
            ),
            Self::Exists => (
                StatusCode::CONFLICT,
                "path_exists",
                "A file or directory already exists at this path",
            ),
            Self::Missing => (
                StatusCode::NOT_FOUND,
                "path_not_found",
                "The file no longer exists",
            ),
            Self::FileChanged => (
                StatusCode::CONFLICT,
                "file_changed",
                "The file changed; reload before committing",
            ),
            Self::Unsupported => (
                StatusCode::BAD_REQUEST,
                "unsupported_file",
                "Only regular files can be changed in the browser",
            ),
            Self::Unchanged => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "unchanged",
                "The file content is unchanged",
            ),
            Self::NotDirectory => (
                StatusCode::CONFLICT,
                "not_directory",
                "A path component is not a directory",
            ),
            Self::Receive(error) if matches!(error.as_ref(), ReceiveError::Busy) => (
                StatusCode::TOO_MANY_REQUESTS,
                "busy",
                "Git writes are busy; retry shortly",
            ),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                "publication_failed",
                "The file could not be committed. Reload the repository before retrying",
            ),
        };
        if status.is_server_error() {
            tracing::error!(error = ?self, "browser file publication failed");
        }
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

struct BuiltCommit {
    oid: ObjectId,
    objects: Vec<(Kind, Vec<u8>)>,
}

enum BuildOutcome {
    Committed(BuiltCommit),
    Exists,
    Missing,
    FileChanged,
    Unsupported,
    Unchanged,
    NotDirectory,
}

#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error("repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object decoding failed")]
    Decode(#[from] gix_object::decode::Error),
    #[error("Git object encoding failed")]
    Io(#[from] std::io::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<CreateInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    change(server, principal, owner, name, ChangeInput::Create(input)).await
}

async fn update(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<UpdateInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    change(server, principal, owner, name, ChangeInput::Update(input)).await
}

async fn remove(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<DeleteInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    change(server, principal, owner, name, ChangeInput::Delete(input)).await
}

async fn upload(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name)): Path<(String, String)>,
    input: std::result::Result<Json<UploadInput>, JsonRejection>,
) -> Result<impl IntoResponse, Error> {
    let Json(input) = input?;
    let repo = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::Permission);
    }
    validate_branch(&input.branch)?;
    let publication_branch = publication_branch(&input.branch, input.new_branch.as_deref())?;
    validate_message(&input.message)?;
    let expected = parse_oid(
        &input.expected_head,
        "Expected head must be a full SHA-1 commit ID",
    )?;
    let upload = validate_upload(&input.files)?;
    let actor = app::actor(&principal)?;
    let cancellation = server.cancellation.child_token();
    let repository = repo
        .open_current(&server, server.options, &cancellation)
        .await?;
    let current = repository
        .refs()
        .entries
        .iter()
        .find(|reference| reference.name == input.branch)
        .map(|reference| reference.target)
        .ok_or(Error::Conflict)?;
    if current != expected {
        return Err(Error::Conflict);
    }
    if publication_branch != input.branch
        && repository
            .refs()
            .entries
            .iter()
            .any(|reference| reference.name == publication_branch)
    {
        return Err(Error::BranchExists);
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, &cancellation)
        .await?;
    let built = build_upload_commit(
        &repository,
        &operation,
        expected,
        &upload.tree,
        &actor,
        seconds,
        input.message.trim(),
    )
    .await;
    let built = finish_build(operation, built).await?;
    let built = match built {
        BuildOutcome::Committed(built) => built,
        BuildOutcome::Exists => return Err(Error::Exists),
        BuildOutcome::NotDirectory => return Err(Error::NotDirectory),
        BuildOutcome::Missing | BuildOutcome::FileChanged | BuildOutcome::Unsupported => {
            return Err(Error::Unsupported);
        }
        BuildOutcome::Unchanged => return Err(Error::Unchanged),
    };
    receive::publish_objects(
        Arc::clone(&server),
        principal,
        (owner, name),
        crab_git::receive_plan::RefUpdate {
            name: publication_branch.clone(),
            old: (publication_branch == input.branch).then_some(expected),
            new: Some(built.oid),
        },
        built.objects,
        visibility_base(&input.branch, &publication_branch, expected),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(UploadOutput {
            branch: publication_branch,
            commit: built.oid.to_string(),
            paths_hex: upload.paths_hex,
        }),
    ))
}

async fn change(
    server: Arc<Server>,
    principal: Principal,
    owner: String,
    name: String,
    input: ChangeInput,
) -> Result<impl IntoResponse, Error> {
    let repo = app::repository(&server, &principal, &(owner.clone(), name.clone()))?;
    if !principal.can_write(&repo.config) {
        return Err(Error::Permission);
    }
    let path = validate_input(&input)?;
    let publication_branch = publication_branch(input.branch(), input.new_branch())?;
    let expected = parse_oid(
        input.expected_head(),
        "Expected head must be a full SHA-1 commit ID",
    )?;
    let actor = app::actor(&principal)?;
    let cancellation = server.cancellation.child_token();
    let repository = repo
        .open_current(&server, server.options, &cancellation)
        .await?;
    let current = repository
        .refs()
        .entries
        .iter()
        .find(|reference| reference.name == input.branch())
        .map(|reference| reference.target)
        .ok_or(Error::Conflict)?;
    if current != expected {
        return Err(Error::Conflict);
    }
    if publication_branch != input.branch()
        && repository
            .refs()
            .entries
            .iter()
            .any(|reference| reference.name == publication_branch)
    {
        return Err(Error::BranchExists);
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, &cancellation)
        .await?;
    let built = build_commit(
        &repository,
        &operation,
        expected,
        &path,
        &input,
        &actor,
        seconds,
    )
    .await;
    let built = finish_build(operation, built).await?;
    let built = match built {
        BuildOutcome::Committed(built) => built,
        BuildOutcome::Exists => return Err(Error::Exists),
        BuildOutcome::Missing => return Err(Error::Missing),
        BuildOutcome::FileChanged => return Err(Error::FileChanged),
        BuildOutcome::Unsupported => return Err(Error::Unsupported),
        BuildOutcome::Unchanged => return Err(Error::Unchanged),
        BuildOutcome::NotDirectory => return Err(Error::NotDirectory),
    };
    let status = input.status();
    receive::publish_objects(
        Arc::clone(&server),
        principal,
        (owner, name),
        crab_git::receive_plan::RefUpdate {
            name: publication_branch.clone(),
            old: (publication_branch == input.branch()).then_some(expected),
            new: Some(built.oid),
        },
        built.objects,
        visibility_base(input.branch(), &publication_branch, expected),
    )
    .await?;
    Ok((
        status,
        Json(ChangeOutput {
            branch: publication_branch,
            commit: built.oid.to_string(),
            path_hex: input.path_hex().to_ascii_lowercase(),
        }),
    ))
}

fn validate_input(input: &ChangeInput) -> Result<crab_remote_git::GitPath, Error> {
    validate_branch(input.branch())?;
    let path = validate_path(input.path_hex())?;
    if input
        .content()
        .is_some_and(|content| content.len() > MAX_CONTENT_BYTES)
    {
        return Err(Error::Input("File content must be 900 KiB or smaller"));
    }
    validate_message(input.message())?;
    parse_oid(
        input.expected_head(),
        "Expected head must be a full SHA-1 commit ID",
    )?;
    if let Some(expected_blob) = input.expected_blob() {
        parse_oid(
            expected_blob,
            "Expected blob must be a full SHA-1 object ID",
        )?;
    }
    Ok(path)
}

fn validate_branch(branch: &str) -> Result<(), Error> {
    if !branch.starts_with("refs/heads/") || crab_git::validate_push_refname(branch).is_err() {
        return Err(Error::Input("Select an existing branch"));
    }
    Ok(())
}

fn publication_branch(source: &str, new_branch: Option<&str>) -> Result<String, Error> {
    let Some(name) = new_branch else {
        return Ok(source.to_owned());
    };
    let branch = crate::branches::branch_ref(name).map_err(Error::Input)?;
    if branch == source {
        return Err(Error::Input(
            "New branch must differ from the source branch",
        ));
    }
    Ok(branch)
}

fn visibility_base(
    source: &str,
    destination: &str,
    expected: ObjectId,
) -> Option<(String, ObjectId)> {
    // A new destination has no old OID for its absent-ref comparison. Carry the
    // exact source separately so visibility validation can reuse its proven closure.
    (destination != source).then(|| (source.to_owned(), expected))
}

fn validate_path(path_hex: &str) -> Result<crab_remote_git::GitPath, Error> {
    if path_hex.is_empty()
        || !path_hex.len().is_multiple_of(2)
        || path_hex.len() > MAX_PATH_BYTES * 2
    {
        return Err(Error::Input("Enter a valid repository path"));
    }
    let bytes = (0..path_hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&path_hex[index..index + 2], 16))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::Input("Enter a valid repository path"))?;
    let path = crab_remote_git::GitPath::new(bytes)?;
    if path.is_root()
        || path.components().any(|part| {
            matches!(part, b"." | b"..") || part.eq_ignore_ascii_case(b".git") || part.len() > 255
        })
    {
        return Err(Error::Input("Enter a valid repository path"));
    }
    Ok(path)
}

fn validate_message(message: &str) -> Result<(), Error> {
    let message = message.trim();
    if message.is_empty()
        || message.chars().count() > MAX_MESSAGE_CHARS
        || message
            .chars()
            .any(|character| character.is_control() && character != '\n')
    {
        return Err(Error::Input("Commit message must contain 1–256 characters"));
    }
    Ok(())
}

#[derive(Default)]
struct UploadTree {
    content: Option<Vec<u8>>,
    children: BTreeMap<Vec<u8>, UploadTree>,
}

struct ValidatedUpload {
    tree: UploadTree,
    paths_hex: Vec<String>,
}

fn validate_upload(files: &[UploadFileInput]) -> Result<ValidatedUpload, Error> {
    if files.is_empty() || files.len() > MAX_UPLOAD_FILES {
        return Err(Error::Input("Select 1–100 files to upload"));
    }
    let mut total = 0usize;
    let mut tree = UploadTree::default();
    let mut paths_hex = Vec::with_capacity(files.len());
    for file in files {
        let path = validate_path(&file.path_hex)?;
        let content = base64::engine::general_purpose::STANDARD
            .decode(&file.content_base64)
            .map_err(|_| Error::Input("Uploaded file content must be valid base64"))?;
        if content.len() > MAX_CONTENT_BYTES {
            return Err(Error::Input(
                "Each uploaded file must be 900 KiB or smaller",
            ));
        }
        total = total
            .checked_add(content.len())
            .filter(|total| *total <= MAX_UPLOAD_BYTES)
            .ok_or(Error::Input("Uploaded files must total 4 MiB or smaller"))?;
        let mut node = &mut tree;
        for component in path.components() {
            if node.content.is_some() {
                return Err(Error::Input("Uploaded file paths cannot overlap"));
            }
            node = node.children.entry(component.to_vec()).or_default();
        }
        if node.content.is_some() || !node.children.is_empty() {
            return Err(Error::Input("Uploaded file paths cannot overlap"));
        }
        node.content = Some(content);
        paths_hex.push(file.path_hex.to_ascii_lowercase());
    }
    Ok(ValidatedUpload { tree, paths_hex })
}

async fn finish_build(
    operation: crab_remote_git::OperationContext,
    built: Result<BuildOutcome, BuildError>,
) -> Result<BuildOutcome, Error> {
    match built {
        Ok(built) => {
            operation.finish(Ok(())).await?;
            Ok(built)
        }
        Err(BuildError::Remote(error)) => match operation.finish::<()>(Err(error)).await {
            Err(error) => Err(Error::Remote(error)),
            Ok(()) => Err(Error::Remote(crab_remote_git::Error::InternalInvariant {
                invariant: "failed repository edit unexpectedly succeeded",
            })),
        },
        Err(BuildError::Decode(error)) => {
            operation.finish(Ok(())).await?;
            Err(Error::Decode(error))
        }
        Err(BuildError::Io(error)) => {
            operation.finish(Ok(())).await?;
            Err(Error::Io(error))
        }
        Err(BuildError::Hash(error)) => {
            operation.finish(Ok(())).await?;
            Err(Error::Hash(error))
        }
    }
}

fn parse_oid(value: &str, message: &'static str) -> Result<ObjectId, Error> {
    ObjectId::from_hex(value.as_bytes())
        .ok()
        .filter(|oid| oid.kind() == gix_hash::Kind::Sha1 && !oid.is_null())
        .ok_or(Error::Input(message))
}

async fn build_commit(
    repository: &crab_remote_git::RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    parent: ObjectId,
    path: &crab_remote_git::GitPath,
    input: &ChangeInput,
    actor: &Identity,
    seconds: u64,
) -> Result<BuildOutcome, BuildError> {
    let snapshot = repository
        .snapshot(&crab_remote_git::Revision::Commit(parent), operation)
        .await?;
    let existing = match snapshot.entry(path, operation).await {
        Ok(entry) => entry,
        Err(crab_remote_git::Error::PathComponentNotTree { .. }) => {
            return Ok(BuildOutcome::NotDirectory);
        }
        Err(error) => return Err(error.into()),
    };
    match (input, &existing) {
        (ChangeInput::Create(_), Some(_)) => return Ok(BuildOutcome::Exists),
        (ChangeInput::Create(_), None) => {}
        (ChangeInput::Update(_) | ChangeInput::Delete(_), None) => {
            return Ok(BuildOutcome::Missing);
        }
        (ChangeInput::Update(_) | ChangeInput::Delete(_), Some(entry))
            if entry.kind != crab_remote_git::EntryKind::Blob =>
        {
            return Ok(BuildOutcome::Unsupported);
        }
        (ChangeInput::Update(_) | ChangeInput::Delete(_), Some(entry)) => {
            let expected = ObjectId::from_hex(input.expected_blob().unwrap_or_default().as_bytes())
                .map_err(|_| crab_remote_git::Error::InternalInvariant {
                    invariant: "validated browser mutation had an invalid expected blob",
                })?;
            if entry.oid != expected {
                return Ok(BuildOutcome::FileChanged);
            }
        }
    }
    let mut levels = Vec::new();
    let components = path.components().collect::<Vec<_>>();
    let (file_name, directories) =
        components
            .split_last()
            .ok_or(crab_remote_git::Error::InternalInvariant {
                invariant: "validated file path had no component",
            })?;
    let mut current_tree = Some(snapshot.root_tree_oid());
    for component in directories {
        let entries = match current_tree {
            Some(oid) => read_tree(operation, oid).await?,
            None => Vec::new(),
        };
        current_tree = match entries
            .iter()
            .find(|entry| entry.filename.as_slice() == *component)
        {
            Some(entry) if entry.mode.is_tree() => Some(entry.oid),
            Some(_) => return Ok(BuildOutcome::NotDirectory),
            None => None,
        };
        levels.push((entries, (*component).to_vec()));
    }
    let mut entries = match current_tree {
        Some(oid) => read_tree(operation, oid).await?,
        None => Vec::new(),
    };
    let mut objects = Vec::new();
    let position = entries
        .iter()
        .position(|entry| entry.filename.as_slice() == *file_name);
    match input {
        ChangeInput::Create(input) => {
            if position.is_some() {
                return Ok(BuildOutcome::Exists);
            }
            let blob = input.content.as_bytes().to_vec();
            let oid = object_id(Kind::Blob, &blob)?;
            objects.push((Kind::Blob, blob));
            entries.push(tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: BString::from((*file_name).to_vec()),
                oid,
            });
        }
        ChangeInput::Update(input) => {
            let position = position.ok_or(crab_remote_git::Error::InternalInvariant {
                invariant: "resolved browser edit disappeared from its parent tree",
            })?;
            let blob = input.content.as_bytes().to_vec();
            let oid = object_id(Kind::Blob, &blob)?;
            if entries[position].oid == oid {
                return Ok(BuildOutcome::Unchanged);
            }
            objects.push((Kind::Blob, blob));
            entries[position].oid = oid;
        }
        ChangeInput::Delete(_) => {
            let position = position.ok_or(crab_remote_git::Error::InternalInvariant {
                invariant: "resolved browser deletion disappeared from its parent tree",
            })?;
            entries.remove(position);
        }
    }
    let deleting = matches!(input, ChangeInput::Delete(_));
    let mut tree_oid = if deleting && entries.is_empty() && !levels.is_empty() {
        None
    } else {
        Some(encode_tree(entries, &mut objects)?)
    };
    let level_count = levels.len();
    for (index, (mut entries, component)) in levels.into_iter().rev().enumerate() {
        let position = entries
            .iter()
            .position(|entry| entry.filename.as_slice() == component);
        match (position, tree_oid) {
            (Some(position), Some(oid)) => entries[position].oid = oid,
            (Some(position), None) => {
                entries.remove(position);
            }
            (None, Some(oid)) => entries.push(tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: BString::from(component),
                oid,
            }),
            (None, None) => {
                return Err(crab_remote_git::Error::InternalInvariant {
                    invariant: "deleted browser path had a missing parent tree entry",
                }
                .into());
            }
        }
        let is_root = index + 1 == level_count;
        tree_oid = if deleting && entries.is_empty() && !is_root {
            None
        } else {
            Some(encode_tree(entries, &mut objects)?)
        };
    }
    let tree_oid = tree_oid.ok_or(crab_remote_git::Error::InternalInvariant {
        invariant: "browser mutation did not produce a root tree",
    })?;
    let commit = commit_bytes(tree_oid, parent, actor, input.message().trim(), seconds);
    let oid = object_id(Kind::Commit, &commit)?;
    objects.push((Kind::Commit, commit));
    Ok(BuildOutcome::Committed(BuiltCommit { oid, objects }))
}

async fn build_upload_commit(
    repository: &crab_remote_git::RemoteGitRepository,
    operation: &crab_remote_git::OperationContext,
    parent: ObjectId,
    upload: &UploadTree,
    actor: &Identity,
    seconds: u64,
    message: &str,
) -> Result<BuildOutcome, BuildError> {
    let snapshot = repository
        .snapshot(&crab_remote_git::Revision::Commit(parent), operation)
        .await?;
    let mut objects = Vec::new();
    let root = match build_upload_tree(
        operation,
        Some(snapshot.root_tree_oid()),
        upload,
        &mut objects,
    )
    .await?
    {
        UploadTreeOutcome::Built(oid) => oid,
        UploadTreeOutcome::Exists => return Ok(BuildOutcome::Exists),
        UploadTreeOutcome::NotDirectory => return Ok(BuildOutcome::NotDirectory),
    };
    let commit = commit_bytes(root, parent, actor, message, seconds);
    let oid = object_id(Kind::Commit, &commit)?;
    objects.push((Kind::Commit, commit));
    Ok(BuildOutcome::Committed(BuiltCommit { oid, objects }))
}

enum UploadTreeOutcome {
    Built(ObjectId),
    Exists,
    NotDirectory,
}

fn build_upload_tree<'a>(
    operation: &'a crab_remote_git::OperationContext,
    current: Option<ObjectId>,
    upload: &'a UploadTree,
    objects: &'a mut Vec<(Kind, Vec<u8>)>,
) -> futures_util::future::BoxFuture<'a, Result<UploadTreeOutcome, BuildError>> {
    Box::pin(async move {
        let mut entries = match current {
            Some(oid) => read_tree(operation, oid).await?,
            None => Vec::new(),
        };
        for (name, child) in &upload.children {
            let position = entries
                .iter()
                .position(|entry| entry.filename.as_slice() == name);
            if let Some(content) = &child.content {
                if position.is_some() {
                    return Ok(UploadTreeOutcome::Exists);
                }
                let oid = object_id(Kind::Blob, content)?;
                objects.push((Kind::Blob, content.clone()));
                entries.push(tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: BString::from(name.clone()),
                    oid,
                });
                continue;
            }
            let existing = match position {
                Some(position) if entries[position].mode.is_tree() => Some(entries[position].oid),
                Some(_) => return Ok(UploadTreeOutcome::NotDirectory),
                None => None,
            };
            let oid = match build_upload_tree(operation, existing, child, objects).await? {
                UploadTreeOutcome::Built(oid) => oid,
                outcome => return Ok(outcome),
            };
            match position {
                Some(position) => entries[position].oid = oid,
                None => entries.push(tree::Entry {
                    mode: tree::EntryKind::Tree.into(),
                    filename: BString::from(name.clone()),
                    oid,
                }),
            }
        }
        Ok(UploadTreeOutcome::Built(encode_tree(entries, objects)?))
    })
}

async fn read_tree(
    operation: &crab_remote_git::OperationContext,
    oid: ObjectId,
) -> Result<Vec<tree::Entry>, BuildError> {
    let object = operation.read_object(oid).await?;
    if object.kind != Kind::Tree {
        return Err(crab_remote_git::Error::InternalInvariant {
            invariant: "commit tree path resolved to a non-tree object",
        }
        .into());
    }
    gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map(gix_object::TreeRef::into_owned)
        .map(|tree| tree.entries)
        .map_err(BuildError::from)
}

fn encode_tree(
    mut entries: Vec<tree::Entry>,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<ObjectId, BuildError> {
    entries.sort();
    let tree = gix_object::Tree { entries };
    let mut bytes = Vec::new();
    tree.write_to(&mut bytes)?;
    let oid = object_id(Kind::Tree, &bytes)?;
    objects.push((Kind::Tree, bytes));
    Ok(oid)
}

fn object_id(kind: Kind, bytes: &[u8]) -> Result<ObjectId, gix_hash::hasher::Error> {
    gix_object::compute_hash(gix_hash::Kind::Sha1, kind, bytes)
}

fn commit_bytes(
    tree: ObjectId,
    parent: ObjectId,
    actor: &Identity,
    message: &str,
    seconds: u64,
) -> Vec<u8> {
    let name: String = actor
        .name
        .chars()
        .filter(|character| !matches!(character, '<' | '>' | '\n' | '\r' | '\0'))
        .take(160)
        .collect();
    let name = if name.trim().is_empty() {
        "Crab user"
    } else {
        name.trim()
    };
    let email_key = blake3::hash(format!("{}\0{}", actor.issuer, actor.subject).as_bytes());
    format!(
        "tree {tree}\nparent {parent}\nauthor {name} <{}@users.crab.invalid> {seconds} +0000\ncommitter {name} <{}@users.crab.invalid> {seconds} +0000\n\n{message}\n",
        email_key.to_hex(),
        email_key.to_hex(),
    )
    .into_bytes()
}
