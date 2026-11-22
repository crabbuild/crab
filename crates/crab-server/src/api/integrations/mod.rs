use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, Method, StatusCode},
    middleware,
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use super::{
    checks::{
        CheckConclusion, CheckStatus, check_conclusion, check_status, conclusion_code, status_code,
    },
    pulls, statuses,
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{CheckOutputRecord, CheckReportInput, integrations as stored},
    repository::Repository,
    server::Server,
};

pub(super) mod dispatch;
mod github;
pub(crate) use dispatch::ci_git_path;
pub(crate) use github::GitHubActions;

const FORK_MIRROR_PREFIX: &str = "crab/pulls/";
const CANDIDATE_MIRROR_PREFIX: &str = "crab/merge-queue/";

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/settings/integrations",
            get(settings).post(edit).layer(DefaultBodyLimit::max(2048)),
        )
        .route(
            "/api/repos/{owner}/{name}/settings/integrations/{connection}/deliveries",
            get(delivery_history),
        )
        .route(
            "/webhooks/github-actions/{repository}/{connection}",
            post(deliver).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .merge(dispatch::routes())
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

pub(crate) fn webhook_path(method: &Method, path: &str) -> bool {
    *method == Method::POST
        && path
            .strip_prefix("/webhooks/github-actions/")
            .is_some_and(|tail| {
                tail.split_once('/')
                    .is_some_and(|(repository, connection)| {
                        Uuid::parse_str(repository).is_ok() && Uuid::parse_str(connection).is_ok()
                    })
            })
}

async fn connections(server: &Server, repo: &Repository) -> Result<Vec<stored::Connection>> {
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<stored::List>(cell.target, None, ())
            .await,
    )
}

pub(super) async fn fork_mirror_ref(
    server: &Server,
    repo: &Repository,
    number: u64,
) -> Result<Option<String>> {
    mirror_ref(server, repo, number, FORK_MIRROR_PREFIX).await
}

pub(super) async fn candidate_mirror_ref(
    server: &Server,
    repo: &Repository,
    number: u64,
    commit: gix_hash::ObjectId,
    queued: bool,
) -> Result<Option<String>> {
    if queued
        && !connections(server, repo)
            .await?
            .iter()
            .any(|row| row.dispatch.is_none())
    {
        return Ok(None);
    }
    Ok(mirror_ref(server, repo, number, CANDIDATE_MIRROR_PREFIX)
        .await?
        .map(|reference| format!("{reference}/{commit}")))
}

async fn mirror_ref(
    server: &Server,
    repo: &Repository,
    number: u64,
    prefix: &str,
) -> Result<Option<String>> {
    if shared_key(server).is_none() || connections(server, repo).await?.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("refs/heads/{prefix}{}/{number}", repo.id)))
}

pub(super) async fn required(
    server: &Server,
    repo: &Repository,
    oid: &str,
    connections: &[Uuid],
    queued_candidate: bool,
) -> Result<Vec<stored::RequiredConnection>> {
    if connections.is_empty() || shared_key(server).is_none() {
        return Ok(Vec::new());
    }
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<stored::ReadRequirements>(
                cell.target,
                None,
                stored::Requirements {
                    oid: oid.to_owned(),
                    connections: connections.to_vec(),
                    queued_candidate,
                },
            )
            .await,
    )
}

fn admin(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<Arc<Repository>> {
    let repo = app::repository(server, principal, key)?;
    if !(principal.is_user() || principal.is_local()) || !principal.can_admin(repo.id) {
        return Err(Error::NotFound);
    }
    Ok(repo)
}

fn view(server: &Server, repo: &Repository, connection: &stored::Connection) -> Value {
    let origin = server
        .auth
        .as_ref()
        .map(|auth| auth.origin())
        .unwrap_or_default();
    json!({
        "id":connection.id, "name":connection.name, "github_repository":connection.github_repository,
        "workflow":connection.workflow, "credential_version":connection.credential,
        "dispatch":connection.dispatch,
        "url":format!("{origin}/webhooks/github-actions/{}/{}", repo.id, connection.id),
    })
}

async fn settings(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    let rows = connections(&server, &repo).await?;
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    let items = rows
        .iter()
        .map(|row| view(&server, &repo, row))
        .collect::<Vec<_>>();
    let dispatch_available = server
        .github_actions
        .as_ref()
        .is_some_and(|github| github.has_binding(repo.id));
    Ok(Json(
        json!({"available": shared_key(&server).is_some(), "dispatch_available": dispatch_available, "items":items}),
    ))
}

#[derive(Serialize)]
struct DeliveryView {
    id: Uuid,
    received_at_ms: u64,
    oid: String,
    run_number: u64,
    attempt: u64,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    details_url: Option<String>,
    updated_check: bool,
}

async fn delivery_history(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, connection)): Path<(String, String, Uuid)>,
) -> Result<Json<Vec<DeliveryView>>> {
    let repo = admin(&server, &principal, &(owner, name))?;
    let cell = app::repository_cell(&server, &repo)?;
    let recent = app::query_output(
        cell.client
            .query::<stored::DeliveryHistory>(cell.target, None, connection)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    let items = recent
        .into_iter()
        .map(|row| {
            Ok(DeliveryView {
                id: row.id,
                received_at_ms: row.received_at_ms,
                oid: row.oid,
                run_number: row.run_number,
                attempt: row.attempt,
                status: check_status(row.status)?,
                conclusion: row.conclusion.map(check_conclusion).transpose()?,
                details_url: row.details_url,
                updated_check: row.updated_check,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Json(items))
}

fn shared_key(server: &Server) -> Option<&[u8; 32]> {
    server.auth.as_ref()?.webhook_key()
}

fn secret(key: &[u8; 32], repository: Uuid, connection: &stored::Connection) -> String {
    let mut hash = blake3::Hasher::new_keyed(key);
    hash.update(repository.as_bytes());
    hash.update(connection.id.as_bytes());
    hash.update(connection.credential.as_bytes());
    hash.finalize().to_hex().to_string()
}

async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Json(change): Json<stored::Change>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    let key = shared_key(&server);
    if key.is_none() && !matches!(&change, stored::Change::Disconnect { .. }) {
        return Err(Error::Invalid(
            "GitHub Actions requires a shared authentication state key",
        ));
    }
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    let dispatch_edit = matches!(&change, stored::Change::Dispatch { .. });
    if let stored::Change::Dispatch {
        id,
        dispatch: Some(config),
        ..
    } = &change
    {
        if !config.valid() {
            return Err(Error::Invalid(
                "Enter a GitHub owner/repository, installation ID and an exact workflow branch",
            ));
        }
        let github = server.github_actions.as_ref().ok_or(Error::Invalid(
            "Configure the GitHub App on this server to start queue workflows",
        ))?;
        let connection = connections(&server, &repo)
            .await?
            .into_iter()
            .find(|connection| connection.id == *id)
            .ok_or(Error::NotFound)?;
        if !github.permits(repo.id, &connection, config) {
            return Err(Error::Invalid(
                "The server operator must authorize this GitHub workflow and branch for this repository",
            ));
        }
    }
    let cell = app::repository_cell(&server, &repo)?;
    let outcome = app::command_output(
        cell.client
            .command::<stored::Edit>(cell.target, app::mutation_identity()?, change)
            .await,
    )?;
    match outcome {
        stored::Changed::Saved(row) => {
            let key = key.ok_or(Error::CellContract("CI credential root is missing"))?;
            let fetch_token = dispatch::fetch_secret(key, repo.id, &row)?;
            let mut result = json!({"connection":view(&server, &repo, &row)});
            if let Some(token) = fetch_token {
                result["fetch_token"] = json!(token);
            }
            if !dispatch_edit {
                result["secret"] = json!(secret(key, repo.id, &row));
            }
            Ok(Json(result))
        }
        stored::Changed::Disconnected => Ok(Json(json!({"disconnected":true}))),
        stored::Changed::Conflict => Err(Error::Conflict),
        stored::Changed::NotFound => Err(Error::NotFound),
        stored::Changed::AtCapacity => Err(Error::Invalid(
            "A repository can connect at most 16 workflows",
        )),
        stored::Changed::Archived => Err(Error::Archived),
    }
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    repository: GitHubRepository,
    workflow_run: WorkflowRun,
}

#[derive(Deserialize)]
struct GitHubRepository {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
struct WorkflowRun {
    id: u64,
    workflow_id: u64,
    run_number: u64,
    run_attempt: u64,
    head_sha: String,
    head_branch: Option<String>,
    display_title: Option<String>,
    actor: Option<GitHubActor>,
    head_repository: Option<GitHubHeadRepository>,
    event: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
struct GitHubActor {
    login: String,
}

#[derive(Deserialize)]
struct GitHubHeadRepository {
    id: u64,
}

fn mirror_pull(repo: Uuid, branch: &str, prefix: &str) -> Result<u64> {
    let path = branch.strip_prefix(prefix).ok_or(Error::CommitNotFound)?;
    let (upstream, raw) = path.split_once('/').ok_or(Error::CommitNotFound)?;
    let number = raw.parse::<u64>().map_err(|_| Error::CommitNotFound)?;
    if upstream != repo.to_string() || raw != number.to_string() {
        return Err(Error::CommitNotFound);
    }
    app::number(number).map_err(|_| Error::CommitNotFound)
}

fn one_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn verify(secret: &str, headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let signature = one_header(headers, "x-hub-signature-256")
        .and_then(|raw| raw.strip_prefix("sha256="))
        .and_then(|raw| blake3::Hash::from_hex(raw).ok())
        .ok_or(Error::Forbidden)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| Error::CellContract("Invalid webhook key"))?;
    mac.update(body);
    mac.verify_slice(signature.as_bytes())
        .map_err(|_| Error::Forbidden)
}

fn report_state(action: &str, run: &WorkflowRun) -> Result<(CheckStatus, Option<CheckConclusion>)> {
    let (status, conclusion) = match (action, run.status.as_str(), run.conclusion.as_deref()) {
        ("requested", "queued" | "requested", None) => (CheckStatus::Queued, None),
        ("in_progress", "in_progress", None) => (CheckStatus::InProgress, None),
        ("completed", "completed", Some(conclusion)) => (
            CheckStatus::Completed,
            Some(match conclusion {
                "success" => CheckConclusion::Success,
                "action_required" => CheckConclusion::ActionRequired,
                "cancelled" => CheckConclusion::Cancelled,
                "neutral" => CheckConclusion::Neutral,
                "skipped" => CheckConclusion::Skipped,
                "timed_out" => CheckConclusion::TimedOut,
                // Unknown terminal outcomes never satisfy a protected branch.
                _ => CheckConclusion::Failure,
            }),
        ),
        _ => return Err(Error::Invalid("Invalid GitHub workflow status")),
    };
    Ok((status, conclusion))
}

fn run_url(repo: &GitHubRepository, id: u64) -> Result<String> {
    let (owner, name) = repo
        .full_name
        .split_once('/')
        .ok_or(Error::Invalid("Invalid GitHub repository name"))?;
    if ![owner, name]
        .into_iter()
        .all(crate::config::valid_repository_component)
    {
        return Err(Error::Invalid("Invalid GitHub repository name"));
    }
    Ok(format!(
        "https://github.com/{owner}/{name}/actions/runs/{id}"
    ))
}

async fn deliver(
    State(server): State<Arc<Server>>,
    Path((repository, connection)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let root = shared_key(&server).ok_or(Error::NotFound)?;
    let repo = server
        .repositories
        .by_id(repository)
        .ok_or(Error::NotFound)?;
    let row = connections(&server, &repo)
        .await?
        .into_iter()
        .find(|row| row.id == connection)
        .ok_or(Error::NotFound)?;
    verify(&secret(root, repository, &row), &headers, &body)?;
    let event =
        one_header(&headers, "x-github-event").ok_or(Error::Invalid("GitHub event is required"))?;
    // App-level pings can arrive before the app has a repository installation.
    if event != "workflow_run" {
        return Ok(StatusCode::NO_CONTENT);
    }
    let payload: Payload = serde_json::from_slice(&body)
        .map_err(|_| Error::Invalid("Invalid GitHub webhook payload"))?;
    if payload.repository.id != row.github_repository {
        return Err(Error::NotFound);
    }
    let run = payload.workflow_run;
    if run.workflow_id != row.workflow
        || !matches!(run.event.as_str(), "push" | "workflow_dispatch")
    {
        return Ok(StatusCode::NO_CONTENT);
    }
    let (status, conclusion) = report_state(&payload.action, &run)?;
    let id = one_header(&headers, "x-github-delivery")
        .and_then(|raw| Uuid::parse_str(raw).ok())
        .ok_or(Error::Invalid("GitHub delivery ID is required"))?;
    let delivery = stored::Delivery {
        connection,
        id,
        digest: *blake3::hash(&body).as_bytes(),
    };
    let cell = app::repository_cell(&server, &repo)?;
    if let Some(matches) = app::query_output(
        cell.client
            .query::<stored::Receipt>(cell.target, None, delivery.clone())
            .await,
    )? {
        return if matches {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::RequestConflict)
        };
    }
    let (oid, dispatch) = if run.event == "workflow_dispatch" {
        let Some((oid, dispatch)) = dispatch::authorize_report(&server, &repo, &row, &run).await?
        else {
            return Ok(StatusCode::NO_CONTENT);
        };
        (oid, Some(dispatch))
    } else {
        (push_target(&server, &repo, &row, &run).await?, None)
    };
    let report = CheckReportInput {
        status: status_code(status),
        conclusion: conclusion.map(conclusion_code),
        details_url: Some(run_url(&payload.repository, run.id)?),
        output: CheckOutputRecord {
            title: format!("GitHub Actions workflow run #{}", run.run_number),
            summary: format!(
                "Attempt {} of workflow {} in {}.",
                run.run_attempt, row.workflow, payload.repository.full_name
            ),
            text: None,
            steps: vec![],
            annotations: vec![],
        },
    };
    let outcome = app::command_output(
        cell.client
            .command::<stored::Record>(
                cell.target,
                app::mutation_identity()?,
                stored::Report {
                    delivery,
                    credential: row.credential,
                    github_repository: row.github_repository,
                    workflow: row.workflow,
                    run_id: run.id,
                    run_number: run.run_number,
                    attempt: run.run_attempt,
                    oid,
                    report,
                    dispatch,
                },
            )
            .await,
    )?;
    match outcome {
        stored::Reported::Recorded => Ok(StatusCode::NO_CONTENT),
        stored::Reported::Conflict => Err(Error::RequestConflict),
        stored::Reported::Revoked => Err(Error::NotFound),
        stored::Reported::AtCapacity => {
            Err(Error::Invalid("A commit supports at most 100 check runs"))
        }
        stored::Reported::Archived => Err(Error::Archived),
    }
}

async fn push_target(
    server: &Server,
    repo: &Repository,
    row: &stored::Connection,
    run: &WorkflowRun,
) -> Result<String> {
    let oid = statuses::parse_oid(&run.head_sha)?;
    // A reserved mirror branch must name the exact upstream and fork PR.
    // Check receipts first so an acknowledged delivery survives later ref moves.
    let branch = run.head_branch.as_deref().unwrap_or_default();
    let mirror = [FORK_MIRROR_PREFIX, CANDIDATE_MIRROR_PREFIX]
        .into_iter()
        .find(|prefix| branch.starts_with(prefix));
    Ok(match mirror {
        Some(prefix) => {
            if run.head_repository.as_ref().map(|head| head.id) != Some(row.github_repository) {
                return Err(Error::CommitNotFound);
            }
            if prefix == CANDIDATE_MIRROR_PREFIX {
                let (scope, sha) = branch.rsplit_once('/').ok_or(Error::CommitNotFound)?;
                if sha != oid.to_string() {
                    return Err(Error::CommitNotFound);
                }
                let number = mirror_pull(repo.id, scope, prefix)?;
                let queued =
                    pulls::candidate::require_check_target(server, repo, number, oid).await?;
                if queued && row.dispatch.is_some() {
                    return Err(Error::CommitNotFound);
                }
                oid.to_string()
            } else {
                let number = mirror_pull(repo.id, branch, prefix)?;
                pulls::require_fork_check_head(server, repo, number, oid).await?
            }
        }
        None => {
            let requested = oid.to_string();
            let refs = repo.open_ref_view().await?;
            if refs.refs().iter().any(|(name, tip)| {
                name.starts_with(crate::git::receive::MERGE_CANDIDATE_PREFIX) && *tip == requested
            }) {
                return Err(Error::CommitNotFound);
            }
            statuses::require_commit(server, repo, oid).await?
        }
    })
}

#[cfg(test)]
mod tests;
