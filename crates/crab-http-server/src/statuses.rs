use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::get,
};
use crab_remote_git::{OperationKind, Revision, RevisionError};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{
    app::{self, Error, Result},
    app_storage,
    auth::{Identity, Principal},
    server::{Repository, Server},
};

const ROOT: &str = "app/v1/statuses";
const MAX_CONTEXTS: usize = 128;

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StatusSummary {
    oid: String,
    statuses: Vec<CommitStatus>,
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

fn root(oid: &str) -> String {
    format!("{ROOT}/{oid}")
}

fn summary_path(oid: &str) -> String {
    format!("{}/summary.json", root(oid))
}

fn request_path(oid: &str, request_id: &str) -> String {
    format!("{}/requests/{request_id}.json", root(oid))
}

fn context_key(context: &str) -> String {
    context.to_lowercase()
}

pub(crate) fn same_context(left: &str, right: &str) -> bool {
    context_key(left) == context_key(right)
}

fn parse_oid(value: &str) -> Result<ObjectId> {
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

fn validate_target(value: Option<&str>) -> Result<()> {
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

async fn require_commit(server: &Server, repo: &Repository, oid: ObjectId) -> Result<String> {
    let cancellation = server.cancellation.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let repository = repo.open(server, &cancellation).await?;
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
        && app_storage::same_author(&left.author, &right.author)
        && left.oid == right.oid
        && left.context == right.context
        && left.state == right.state
        && left.description == right.description
        && left.target_url == right.target_url
}

async fn record(repo: &Repository, proposed: CommitStatus) -> Result<CommitStatus> {
    let reservation = request_path(&proposed.oid, &proposed.request_id);
    let status = app_storage::create_or_read(repo, &reservation, proposed.clone()).await?;
    if !same_status(&status, &proposed) {
        return Err(Error::RequestConflict);
    }
    apply(repo, &status).await?;
    Ok(status)
}

async fn apply(repo: &Repository, status: &CommitStatus) -> Result<()> {
    let path = summary_path(&status.oid);
    for _ in 0..10 {
        let Some((mut summary, etag)) = app_storage::read::<StatusSummary>(repo, &path).await?
        else {
            let created = app_storage::create_or_read(
                repo,
                &path,
                StatusSummary {
                    oid: status.oid.clone(),
                    statuses: vec![status.clone()],
                },
            )
            .await?;
            if created.oid != status.oid {
                return Err(Error::Conflict);
            }
            if created.statuses.iter().any(|current| {
                context_key(&current.context) == context_key(&status.context)
                    && current.number >= status.number
            }) {
                return Ok(());
            }
            continue;
        };
        if summary.oid != status.oid {
            return Err(Error::Conflict);
        }
        let key = context_key(&status.context);
        if summary
            .statuses
            .iter()
            .any(|current| context_key(&current.context) == key && current.number >= status.number)
        {
            return Ok(());
        }
        match summary
            .statuses
            .iter()
            .position(|current| context_key(&current.context) == key)
        {
            Some(index) => summary.statuses[index] = status.clone(),
            None if summary.statuses.len() < MAX_CONTEXTS => summary.statuses.push(status.clone()),
            None => {
                return Err(Error::Invalid(
                    "A commit supports at most 128 status contexts",
                ));
            }
        }
        match app_storage::update(repo, &path, &summary, etag).await {
            Ok(()) => return Ok(()),
            Err(Error::Storage(crab_storage::StorageError::StateConflict { .. })) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(Error::Conflict)
}

pub(crate) async fn latest(repo: &Repository, oid: &str) -> Result<Vec<CommitStatus>> {
    let Some((summary, _)) = app_storage::read::<StatusSummary>(repo, &summary_path(oid)).await?
    else {
        return Ok(vec![]);
    };
    if summary.oid != oid {
        return Err(Error::Conflict);
    }
    let mut statuses = summary.statuses;
    statuses.sort_by_cached_key(|status| context_key(&status.context));
    Ok(statuses)
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
    let oid = require_commit(&server, repo, parse_oid(&oid)?).await?;
    let statuses = latest(repo, &oid).await?;
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
    if !principal.can_write(&repo.config) {
        return Err(Error::StatusPermission);
    }
    let Json(input) = input?;
    validate_context(&input.context)?;
    validate_description(input.description.as_deref())?;
    validate_target(input.target_url.as_deref())?;
    let parsed_oid = parse_oid(&oid)?;
    let oid = parsed_oid.to_string();
    let request_id = app::submission(&input.request_id)?;
    let author = app::actor(&principal)?;
    let reservation = request_path(&oid, &request_id);
    if let Some((status, _)) = app_storage::read::<CommitStatus>(repo, &reservation).await? {
        let candidate = CommitStatus {
            number: status.number,
            request_id,
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
        if !principal.can_write(&repo.config) {
            return Err(Error::StatusPermission);
        }
        apply(repo, &status).await?;
        return Ok((StatusCode::CREATED, Json(status_view(&status))));
    }
    let oid = require_commit(&server, repo, parsed_oid).await?;
    if !principal.can_write(&repo.config) {
        return Err(Error::StatusPermission);
    }
    let root = root(&oid);
    let number = app_storage::reserve_number(repo, &root).await?;
    if number > 1_000 {
        return Err(Error::Invalid(
            "A commit supports at most 1,000 status submissions",
        ));
    }
    let status = record(
        repo,
        CommitStatus {
            number,
            request_id,
            author,
            oid,
            context: input.context,
            state: input.state,
            description: input.description,
            target_url: input.target_url,
            created_at: app_storage::now()?,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(status_view(&status))))
}
