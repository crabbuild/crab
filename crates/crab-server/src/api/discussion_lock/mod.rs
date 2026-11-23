use std::sync::Arc;

use super::discussion_target::can_manage;
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::{Subject, SubjectKind, locks},
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .merge(
            Router::new()
                .route(
                    "/api/repos/{owner}/{name}/issues/{number}/lock",
                    get(read).put(write),
                )
                .layer(Extension(SubjectKind::Issue)),
        )
        .merge(
            Router::new()
                .route(
                    "/api/repos/{owner}/{name}/pulls/{number}/lock",
                    get(read).put(write),
                )
                .layer(Extension(SubjectKind::Pull)),
        )
        .layer(DefaultBodyLimit::max(1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Serialize)]
struct View {
    version: u64,
    locked: bool,
    reason: Option<locks::Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_in_ms: Option<u64>,
    can_manage: bool,
    can_post: bool,
}

fn view(lock: locks::Lock, archived: bool, manages: bool) -> Result<Json<View>> {
    let expires_at_ms = if archived { None } else { lock.expires_at_ms() };
    let expires_in_ms = expires_at_ms
        .map(|expires| crate::cells::unix_now_ms().map(|now| expires.saturating_sub(now as u64)))
        .transpose()
        .map_err(Error::Cell)?;
    Ok(Json(View {
        version: lock.version,
        locked: lock.locked || archived,
        reason: lock.reason,
        expires_at_ms,
        expires_in_ms,
        can_manage: !archived && manages,
        can_post: !archived && (!lock.locked || manages),
    }))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let subject = Subject {
        kind,
        number: app::number(number)?,
    };
    let cell = app::repository_cell(&server, &repo)?;
    let (lock, archived) = app::query_output(
        cell.client
            .query::<locks::Get>(&cell.target, None, subject)
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    view(
        lock,
        archived,
        can_manage(&membership.principal, repo.id, kind),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u64,
    change: locks::Change,
}

async fn write(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Extension(kind): Extension<SubjectKind>,
    Path((owner, name, number)): Path<(String, String, u64)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let subject = Subject {
        kind,
        number: app::number(number)?,
    };
    let Json(input) = input?;
    if input.version >= app::MAX_NUMBER {
        return Err(Error::Invalid("Invalid conversation lock version"));
    }
    let actor = app::actor(&principal)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    if !can_manage(&membership.principal, repo.id, kind) {
        return Err(Error::ConversationLockPermission);
    }
    let cell = app::repository_cell(&server, &repo)?;
    match app::command_output(
        cell.client
            .command::<locks::Set>(
                &cell.target,
                app::mutation_identity()?,
                locks::Input {
                    subject,
                    actor: (&actor).into(),
                    version: input.version,
                    change: input.change,
                },
            )
            .await,
    )? {
        locks::Outcome::Saved(lock, archived) => {
            let membership = app::membership(&server, &principal, &repo).await?;
            view(
                lock,
                archived,
                can_manage(&membership.principal, repo.id, kind),
            )
        }
        locks::Outcome::Archived => Err(Error::Archived),
        locks::Outcome::NotFound => Err(Error::NotFound),
        locks::Outcome::Conflict => Err(Error::Conflict),
    }
}

#[cfg(test)]
mod tests;
