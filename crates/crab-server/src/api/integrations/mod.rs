use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    middleware,
    routing::get,
};
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::Sha256;
use uuid::Uuid;

use super::{
    checks::{CheckConclusion, CheckStatus, check_conclusion, check_status},
    pulls, statuses,
};
use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::integrations as stored,
    repository::Repository,
    server::Server,
};

pub(super) mod dispatch;
mod github;
mod outbound;
#[cfg(test)]
mod outbound_tests;
mod webhook;
pub(crate) use dispatch::ci_git_path;
pub(crate) use github::GitHubActions;

pub(crate) use webhook::webhook_path;
use webhook::{APP_WEBHOOK, WorkflowRun, one_header};

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
        .merge(webhook::routes())
        .merge(dispatch::routes())
        .merge(outbound::routes())
        .route_layer(middleware::from_fn_with_state(server, app::admit))
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

fn app_bound(server: &Server, repository: Uuid, github_repository: u64, workflow: u64) -> bool {
    server
        .github_actions
        .as_ref()
        .is_some_and(|github| github.binds(repository, github_repository, workflow))
}

fn view(server: &Server, repo: &Repository, connection: &stored::Connection) -> Value {
    let origin = server
        .auth
        .as_ref()
        .map(|auth| auth.origin())
        .unwrap_or_default();
    let managed = connection.app_managed();
    let path = if managed {
        APP_WEBHOOK.into()
    } else {
        format!("/webhooks/github-actions/{}/{}", repo.id, connection.id)
    };
    json!({
        "id":connection.id, "name":connection.name, "github_repository":connection.github_repository,
        "workflow":connection.workflow, "credential_version":connection.credential,
        "dispatch":connection.dispatch,
        "app_managed": managed,
        "app_bound": app_bound(server, repo.id, connection.github_repository, connection.workflow),
        "url": format!("{origin}{path}"),
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
    Ok(Json(
        json!({"available": shared_key(&server).is_some(), "items": items}),
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

pub(super) fn shared_key(server: &Server) -> Option<&[u8; 32]> {
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
    Json(mut change): Json<stored::Change>,
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
    if let stored::Change::Connect {
        github_repository,
        workflow,
        app_managed,
        ..
    } = &mut change
    {
        *app_managed = app_bound(&server, repo.id, *github_repository, *workflow);
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
            if !dispatch_edit && !row.app_managed() {
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

#[cfg(test)]
mod tests;
