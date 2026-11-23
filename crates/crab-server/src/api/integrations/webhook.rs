use axum::{body::Bytes, http::Method, routing::post};
use serde::Deserialize;

use super::*;
use crate::{
    api::checks::{conclusion_code, status_code},
    cells::repository::{CheckOutputRecord, CheckReportInput},
};

pub(super) const APP_WEBHOOK: &str = "/webhooks/github-actions/app";

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            APP_WEBHOOK,
            post(deliver_app).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        .route(
            "/webhooks/github-actions/{repository}/{connection}",
            post(deliver).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
}

pub(crate) fn webhook_path(method: &Method, path: &str) -> bool {
    *method == Method::POST
        && (path == APP_WEBHOOK
            || path
                .strip_prefix("/webhooks/github-actions/")
                .and_then(|tail| tail.split_once('/'))
                .is_some_and(|(repository, connection)| {
                    Uuid::parse_str(repository).is_ok() && Uuid::parse_str(connection).is_ok()
                }))
}

#[derive(Deserialize)]
struct Payload {
    action: String,
    repository: GitHubRepository,
    installation: Option<GitHubInstallation>,
    workflow_run: WorkflowRun,
}

#[derive(Deserialize)]
struct GitHubInstallation {
    id: u64,
}

#[derive(Deserialize)]
struct GitHubRepository {
    id: u64,
    full_name: String,
}

#[derive(Deserialize)]
pub(super) struct WorkflowRun {
    pub(super) id: u64,
    workflow_id: u64,
    run_number: u64,
    run_attempt: u64,
    head_sha: String,
    pub(super) head_branch: Option<String>,
    pub(super) display_title: Option<String>,
    pub(super) actor: Option<GitHubActor>,
    pub(super) head_repository: Option<GitHubHeadRepository>,
    event: String,
    status: String,
    conclusion: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GitHubActor {
    pub(super) login: String,
}

#[derive(Deserialize)]
pub(super) struct GitHubHeadRepository {
    pub(super) id: u64,
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

pub(super) fn one_header<'a>(headers: &'a HeaderMap, name: &'static str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn verify(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let signature = one_header(headers, "x-hub-signature-256")
        .and_then(|raw| raw.strip_prefix("sha256="))
        .and_then(|raw| blake3::Hash::from_hex(raw).ok())
        .ok_or(Error::Forbidden)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)
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
    // Bound workflows accept only the operator's App signature. A previous
    // per-connection secret cannot impersonate that installation.
    if row.app_managed() {
        return Err(Error::NotFound);
    }
    verify(secret(root, repository, &row).as_bytes(), &headers, &body)?;
    if let Some(incoming) = Incoming::parse(&headers, &body, false)? {
        if incoming.payload.repository.id != row.github_repository {
            return Err(Error::NotFound);
        }
        receive(&server, &repo, &row, &incoming).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn deliver_app(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode> {
    let github = server.github_actions.as_ref().ok_or(Error::NotFound)?;
    verify(github.webhook_secret(), &headers, &body)?;
    let Some(incoming) = Incoming::parse(&headers, &body, true)? else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let payload = &incoming.payload;
    let installation = payload
        .installation
        .as_ref()
        .ok_or(Error::Invalid("GitHub installation ID is required"))?;
    let Some(repository) = github.webhook_target(
        installation.id,
        payload.repository.id,
        payload.workflow_run.workflow_id,
    ) else {
        return Ok(StatusCode::NO_CONTENT);
    };
    let repo = server
        .repositories
        .by_id(repository)
        .ok_or(Error::NotFound)?;
    for row in connections(&server, &repo)
        .await?
        .into_iter()
        .filter(|row| {
            row.app_managed()
                && row.github_repository == payload.repository.id
                && row.workflow == payload.workflow_run.workflow_id
        })
    {
        receive(&server, &repo, &row, &incoming).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

struct Incoming {
    payload: Payload,
    id: Uuid,
    digest: [u8; 32],
    app_managed: bool,
}

impl Incoming {
    fn parse(headers: &HeaderMap, body: &[u8], app_managed: bool) -> Result<Option<Self>> {
        let event = one_header(headers, "x-github-event")
            .ok_or(Error::Invalid("GitHub event is required"))?;
        // GitHub pings the App before it has an installation.
        if event != "workflow_run" {
            return Ok(None);
        }
        let payload: Payload = serde_json::from_slice(body)
            .map_err(|_| Error::Invalid("Invalid GitHub webhook payload"))?;
        if !matches!(
            payload.workflow_run.event.as_str(),
            "push" | "workflow_dispatch"
        ) {
            return Ok(None);
        }
        let id = one_header(headers, "x-github-delivery")
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .ok_or(Error::Invalid("GitHub delivery ID is required"))?;
        Ok(Some(Self {
            payload,
            id,
            digest: *blake3::hash(body).as_bytes(),
            app_managed,
        }))
    }
}

async fn receive(
    server: &Server,
    repo: &Repository,
    row: &stored::Connection,
    incoming: &Incoming,
) -> Result<()> {
    let payload = &incoming.payload;
    let run = &payload.workflow_run;
    if run.workflow_id != row.workflow {
        return Ok(());
    }
    let (status, conclusion) = report_state(&payload.action, run)?;
    let delivery = stored::Delivery {
        connection: row.id,
        id: incoming.id,
        digest: incoming.digest,
        app_managed: incoming.app_managed,
    };
    let cell = app::repository_cell(server, repo)?;
    if let Some(matches) = app::query_output(
        cell.client
            .query::<stored::Receipt>(cell.target, None, delivery.clone())
            .await,
    )? {
        return if matches {
            Ok(())
        } else {
            Err(Error::RequestConflict)
        };
    }
    let (oid, dispatch) = if run.event == "workflow_dispatch" {
        let Some((oid, dispatch)) = dispatch::authorize_report(server, repo, row, run).await?
        else {
            return Ok(());
        };
        (oid, Some(dispatch))
    } else {
        (push_target(server, repo, row, run).await?, None)
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
        stored::Reported::Recorded => Ok(()),
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
