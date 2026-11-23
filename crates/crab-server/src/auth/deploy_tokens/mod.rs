use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Principal, key};
use crate::{
    app::{self, Error, Result},
    cells::repository::deploy_tokens as stored,
    repository::Repository,
    server::Server,
};

const PREFIX: &str = "crab_deploy_";

pub(super) fn recognizes(password: &str) -> bool {
    password.starts_with(PREFIX)
}

pub(super) async fn verify(server: &Server, password: &str) -> Option<Principal> {
    let address = password.strip_prefix(PREFIX)?;
    let (repository, tail) = address.split_once('.')?;
    let (id, secret) = tail.split_once('.')?;
    let repository = Uuid::parse_str(repository).ok()?;
    let id = Uuid::parse_str(id).ok()?;
    blake3::Hash::from_hex(secret).ok()?;
    let repo = server.repositories.by_id(repository)?;
    let cell = app::repository_cell(server, &repo).ok()?;
    let expires_at = cell
        .client
        .query::<stored::Verify>(
            cell.target,
            None,
            stored::Proof {
                id,
                digest: key(password),
            },
        )
        .await
        .ok()?
        .output?;
    Some(Principal::deploy(repository, expires_at))
}

pub(crate) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/settings/deploy-tokens",
            get(list).post(change).layer(DefaultBodyLimit::max(2048)),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn admin(
    server: &Server,
    principal: &Principal,
    key: &(String, String),
) -> Result<Arc<Repository>> {
    let repo = app::repository(server, principal, key)?;
    if !principal.is_user()
        || !app::membership(server, principal, &repo)
            .await?
            .principal
            .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    Ok(repo)
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<stored::Snapshot>> {
    let repo = admin(&server, &principal, &key).await?;
    let cell = app::repository_cell(&server, &repo)?;
    let result = app::query_output(
        cell.client
            .query::<stored::List>(cell.target, None, ())
            .await,
    )?;
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    expected_version: u64,
    edit: Edit,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Edit {
    Create { name: String, days: u16 },
    Revoke { id: Uuid },
}

#[derive(Serialize)]
struct Changed {
    token: Option<String>,
}

async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<(String, String)>,
    Json(input): Json<Input>,
) -> Result<Json<Changed>> {
    let repo = admin(&server, &principal, &address).await?;
    let cell = app::repository_cell(&server, &repo)?;
    let (edit, token) = match input.edit {
        Edit::Create { name, days } => {
            let id = Uuid::now_v7();
            let secret = blake3::Hash::from_bytes(rand::random());
            let token = format!("{PREFIX}{}.{id}.{secret}", repo.id);
            (
                stored::Edit::Create {
                    proof: stored::Proof {
                        id,
                        digest: key(&token),
                    },
                    name: name.trim().to_owned(),
                    days,
                    creator: (&app::actor(&principal)?).into(),
                },
                Some(token),
            )
        }
        Edit::Revoke { id } => (stored::Edit::Revoke { id }, None),
    };
    let saved = app::command_output(
        cell.client
            .command::<stored::Change>(
                cell.target,
                app::mutation_identity()?,
                stored::Input {
                    expected_version: input.expected_version,
                    edit,
                },
            )
            .await,
    )?;
    match saved {
        stored::Outcome::Saved => Ok(Json(Changed { token })),
        stored::Outcome::Stale => Err(Error::PageChanged),
        stored::Outcome::Archived => Err(Error::Archived),
        stored::Outcome::Full => Err(Error::Invalid(
            "Revoke a deploy token before creating another",
        )),
    }
}

#[cfg(test)]
mod tests;
