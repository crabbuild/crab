use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State, rejection::JsonRejection},
    middleware,
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

pub(super) fn can_manage(principal: &Principal, repo: Uuid, kind: SubjectKind) -> bool {
    match kind {
        SubjectKind::Issue => principal.can_moderate(repo),
        SubjectKind::Pull => principal.can_write(repo),
    }
}

#[derive(Serialize)]
struct View {
    #[serde(flatten)]
    lock: locks::Lock,
    can_manage: bool,
    can_post: bool,
}

fn view(lock: locks::Lock, archived: bool, manages: bool) -> Json<View> {
    Json(View {
        lock,
        can_manage: !archived && manages,
        can_post: !archived && (!lock.locked || manages),
    })
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
    Ok(view(
        lock,
        archived,
        can_manage(&membership.principal, repo.id, kind),
    ))
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
            Ok(view(
                lock,
                archived,
                can_manage(&membership.principal, repo.id, kind),
            ))
        }
        locks::Outcome::Archived => Err(Error::Archived),
        locks::Outcome::NotFound => Err(Error::NotFound),
        locks::Outcome::Conflict => Err(Error::Conflict),
    }
}

#[cfg(test)]
mod tests;
