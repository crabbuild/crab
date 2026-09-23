use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::client::{Committed, InvocationError, Observed};
use crab_cell_runtime::identity::RequestId;
use crab_remote_git::{OperationKind, Revision, RevisionError};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::{
        RepositoryCell,
        repository::{
            CommitStatusRecord, CommitStatusSubmissionKey, CreateCommitStatus,
            CreateCommitStatusInput, CreateCommitStatusOutcome, GetCommitStatusSubmission,
            ListCommitStatuses, RepositoryAuthor,
        },
    },
    server::{Repository, Server},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StatusState {
    Error,
    Failure,
    Pending,
    Success,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommitStatus {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub oid: String,
    pub context: String,
    pub state: StatusState,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub created_at: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewStatus {
    request_id: String,
    context: String,
    state: StatusState,
    description: Option<String>,
    target_url: Option<String>,
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/commits/{oid}/status", get(list))
        .route(
            "/api/repos/{owner}/{name}/statuses/{oid}",
            axum::routing::post(create),
        )
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

fn context_key(context: &str) -> String {
    context.to_lowercase()
}

pub(crate) fn same_context(left: &str, right: &str) -> bool {
    context_key(left) == context_key(right)
}

pub(crate) fn parse_oid(value: &str) -> Result<ObjectId> {
    ObjectId::from_hex(value.as_bytes())
        .ok()
        .filter(|oid| !oid.is_null())
        .ok_or(Error::Invalid(
            "Commit status requires an exact SHA-1 commit ID",
        ))
}

fn validate_context(value: &str) -> Result<()> {
    if value.trim() != value
        || value.is_empty()
        || value.chars().count() > 100
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid(
            "Status context must contain 1–100 characters without surrounding whitespace or controls",
        ));
    }
    Ok(())
}

fn validate_description(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|value| value.chars().count() > 140 || value.chars().any(char::is_control))
    {
        return Err(Error::Invalid(
            "Status description must be at most 140 characters without controls",
        ));
    }
    Ok(())
}

pub(crate) fn validate_target(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.len() > 2_048 {
        return Err(Error::Invalid("Status target URL is too long"));
    }
    let url = Url::parse(value).map_err(|_| Error::Invalid("Status target URL is invalid"))?;
    crate::config::validate_identity_url(&url, true)
        .map_err(|_| Error::Invalid("Status target URL must use HTTPS or loopback HTTP"))?;
    Ok(())
}

pub(crate) async fn require_commit(
    server: &Server,
    repo: &Repository,
    oid: ObjectId,
) -> Result<String> {
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let repository = repo
        .open_current(server, server.options, &cancellation)
        .await?;
    let operation = repository
        .operation(OperationKind::Commit, &cancellation)
        .await
        .map_err(crate::Error::from)?;
    let result = async {
        let snapshot = repository
            .snapshot(&Revision::Commit(oid), &operation)
            .await?;
        snapshot.commit(&operation).await
    }
    .await;
    let result = operation.finish(result).await;
    match result {
        Ok(commit) => Ok(commit.oid.to_string()),
        Err(crab_remote_git::Error::Revision {
            reason: RevisionError::NotFound | RevisionError::NotReachable,
        }) => Err(Error::CommitNotFound),
        Err(error) => Err(Error::Repository(error.into())),
    }
}

fn same_status(left: &CommitStatus, right: &CommitStatus) -> bool {
    left.request_id == right.request_id
        && left.author.issuer == right.author.issuer
        && left.author.subject == right.author.subject
        && left.oid == right.oid
        && left.context == right.context
        && left.state == right.state
        && left.description == right.description
        && left.target_url == right.target_url
}

pub(crate) async fn latest(
    server: &Server,
    repo: &Repository,
    principal: &Identity,
    oid: &str,
) -> Result<Vec<CommitStatus>> {
    let routed = route(server, repo, principal, "repository.read").await?;
    let catalog = query_output(
        routed
            .client
            .query::<ListCommitStatuses>(&routed.target, None, oid.to_owned())
            .await,
    )?;
    catalog
        .statuses
        .into_iter()
        .map(status_from_record)
        .collect()
}

fn status_view(status: &CommitStatus) -> Value {
    json!({
        "context": status.context,
        "state": status.state,
        "description": status.description,
        "target_url": status.target_url,
        "author": status.author.name,
        "created_at": status.created_at,
    })
}

fn combined(statuses: &[CommitStatus]) -> StatusState {
    if statuses
        .iter()
        .any(|status| matches!(status.state, StatusState::Error | StatusState::Failure))
    {
        StatusState::Failure
    } else if statuses.is_empty()
        || statuses
            .iter()
            .any(|status| status.state == StatusState::Pending)
    {
        StatusState::Pending
    } else {
        StatusState::Success
    }
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    let oid = require_commit(&server, repo, parse_oid(&oid)?).await?;
    let actor = app::actor(&principal)?;
    let statuses = latest(&server, repo, &actor, &oid).await?;
    Ok(Json(json!({
        "sha": oid,
        "state": combined(&statuses),
        "statuses": statuses.iter().map(status_view).collect::<Vec<_>>(),
    })))
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    input: std::result::Result<Json<NewStatus>, JsonRejection>,
) -> Result<impl IntoResponse> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let repo = repo.as_ref();
    if !principal.can_write(&repo.config) {
        return Err(Error::StatusPermission);
    }
    let Json(input) = input?;
    validate_context(&input.context)?;
    validate_description(input.description.as_deref())?;
    validate_target(input.target_url.as_deref())?;
    let parsed_oid = parse_oid(&oid)?;
    let oid = parsed_oid.to_string();
    let request_id = submission_id(&input.request_id)?;
    let author = app::actor(&principal)?;
    let replay_route = route(&server, repo, &author, "repository.read").await?;
    if let Some(record) = query_output(
        replay_route
            .client
            .query::<GetCommitStatusSubmission>(
                &replay_route.target,
                None,
                CommitStatusSubmissionKey {
                    oid: oid.clone(),
                    submission_id: request_id,
                },
            )
            .await,
    )? {
        let status = status_from_record(record)?;
        let candidate = CommitStatus {
            number: status.number,
            request_id: Uuid::from_bytes(request_id).to_string(),
            author,
            oid,
            context: input.context,
            state: input.state,
            description: input.description,
            target_url: input.target_url,
            created_at: status.created_at,
        };
        if !same_status(&status, &candidate) {
            return Err(Error::RequestConflict);
        }
        let routed = route(&server, repo, &candidate.author, "repository.status.create").await?;
        let outcome = command_output(
            routed
                .client
                .command::<CreateCommitStatus>(
                    &routed.target,
                    mutation_identity()?,
                    CreateCommitStatusInput {
                        submission_id: request_id,
                        author: repository_author(&candidate.author),
                        oid: candidate.oid,
                        context: candidate.context,
                        state: status_state_code(candidate.state),
                        description: candidate.description,
                        target_url: candidate.target_url,
                    },
                )
                .await,
        )?;
        let status = created_status(outcome)?;
        return Ok((StatusCode::CREATED, Json(status_view(&status))));
    }
    let oid = require_commit(&server, repo, parsed_oid).await?;
    if !principal.can_write(&repo.config) {
        return Err(Error::StatusPermission);
    }
    let routed = route(&server, repo, &author, "repository.status.create").await?;
    let outcome = command_output(
        routed
            .client
            .command::<CreateCommitStatus>(
                &routed.target,
                mutation_identity()?,
                CreateCommitStatusInput {
                    submission_id: request_id,
                    author: repository_author(&author),
                    oid,
                    context: input.context,
                    state: status_state_code(input.state),
                    description: input.description,
                    target_url: input.target_url,
                },
            )
            .await,
    )?;
    let status = created_status(outcome)?;
    Ok((StatusCode::CREATED, Json(status_view(&status))))
}

async fn route(
    server: &Server,
    repository: &Repository,
    principal: &Identity,
    action: &'static str,
) -> Result<RepositoryCell> {
    let router = server.repository_cells().ok_or(Error::CellUnavailable)?;
    router
        .route(repository.id, principal, action)
        .await
        .map_err(|error| match error {
            crate::Error::Cell(source) => Error::Cell(source),
            source => Error::Repository(source),
        })
}

fn repository_author(identity: &Identity) -> RepositoryAuthor {
    RepositoryAuthor {
        issuer: identity.issuer.clone(),
        subject: identity.subject.clone(),
        name: identity.name.clone(),
    }
}

fn submission_id(value: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(&app::submission(value)?)
        .map(Uuid::into_bytes)
        .map_err(|_| Error::Invalid("Submission ID must be a UUID"))
}

fn mutation_identity() -> Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms().map_err(Error::Repository)?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(Error::CellContract("Cell request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn status_state_code(state: StatusState) -> u8 {
    match state {
        StatusState::Error => 0,
        StatusState::Failure => 1,
        StatusState::Pending => 2,
        StatusState::Success => 3,
    }
}

fn status_state(code: u8) -> Result<StatusState> {
    match code {
        0 => Ok(StatusState::Error),
        1 => Ok(StatusState::Failure),
        2 => Ok(StatusState::Pending),
        3 => Ok(StatusState::Success),
        _ => Err(Error::CellContract("Cell returned an invalid status state")),
    }
}

fn status_from_record(record: CommitStatusRecord) -> Result<CommitStatus> {
    Ok(CommitStatus {
        number: record.number,
        request_id: Uuid::from_bytes(record.submission_id).to_string(),
        author: Identity {
            issuer: record.author.issuer,
            subject: record.author.subject,
            name: record.author.name,
        },
        oid: record.oid,
        context: record.context,
        state: status_state(record.state)?,
        description: record.description,
        target_url: record.target_url,
        created_at: record.created_at_ms,
    })
}

fn created_status(outcome: CreateCommitStatusOutcome) -> Result<CommitStatus> {
    match outcome {
        CreateCommitStatusOutcome::Created(status) => status_from_record(*status),
        CreateCommitStatusOutcome::RequestConflict => Err(Error::RequestConflict),
        CreateCommitStatusOutcome::ContextLimit => Err(Error::Invalid(
            "A commit supports at most 128 status contexts",
        )),
        CreateCommitStatusOutcome::SubmissionLimit => Err(Error::Invalid(
            "A commit supports at most 1,000 status submissions",
        )),
    }
}

fn command_output<T>(result: std::result::Result<Committed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(committed) => Ok(committed.output),
        Err(InvocationError::Rejected(committed)) => Ok(committed.output),
        Err(InvocationError::Pending(_)) => Err(Error::CellPending),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}

fn query_output<T>(result: std::result::Result<Observed<T>, InvocationError<T>>) -> Result<T> {
    match result {
        Ok(observed) => Ok(observed.output),
        Err(InvocationError::Rejected(_)) => Err(Error::CellContract(
            "Cell query returned a durable rejection",
        )),
        Err(InvocationError::Pending(_)) => Err(Error::CellContract(
            "Cell query returned pending mutation evidence",
        )),
        Err(InvocationError::InvalidPublishedResult { source, .. }) => Err(Error::Cell(*source)),
        Err(InvocationError::NotStarted(source)) => Err(Error::Cell(source)),
    }
}
