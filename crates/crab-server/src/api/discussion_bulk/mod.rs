use super::{members::roster::Roster, write::rejected};
use crate::{
    app::{self, Error},
    auth::Principal,
    cells::repository::{
        assignments::Assignee,
        collaboration::{
            Subject, SubjectKind,
            bulk::{self, Action, Failure, Outcome},
        },
    },
    server::Server,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

type Result<T> = std::result::Result<T, Response>;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/discussions/selection",
            get(select),
        )
        .route(
            "/api/repos/{owner}/{name}/discussions/bulk",
            axum::routing::post(edit),
        )
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionQuery {
    repository_id: uuid::Uuid,
    items: String,
}
async fn select(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(query): Query<SelectionQuery>,
) -> app::Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    if query.repository_id != repo.id {
        return Err(Error::Invalid(
            "Repository identity changed; reload the selection",
        ));
    }
    if !app::membership(&server, &principal, &repo)
        .await?
        .principal
        .can_triage(repo.id)
    {
        return Err(Error::Forbidden);
    }
    if query.items.len() > 8192 {
        return Err(Error::Invalid("Selection is too large"));
    }
    let items = query
        .items
        .split(',')
        .map(|value| {
            let parts: Vec<_> = value.split(':').collect();
            let [kind, number, version] = parts.as_slice() else {
                return Err(Error::Invalid("Invalid discussion selection"));
            };
            let kind = match *kind {
                "issue" => SubjectKind::Issue,
                "pull" => SubjectKind::Pull,
                _ => return Err(Error::Invalid("Invalid discussion kind")),
            };
            Ok(bulk::Versioned {
                subject: Subject {
                    kind,
                    number: number
                        .parse()
                        .map_err(|_| Error::Invalid("Invalid discussion number"))?,
                },
                version: version
                    .parse()
                    .map_err(|_| Error::Invalid("Invalid discussion version"))?,
            })
        })
        .collect::<app::Result<Vec<_>>>()?;
    let routed = app::repository_cell(&server, &repo)?;
    let selected = app::query_output(
        routed
            .client
            .query::<bulk::Select>(&routed.target, None, items)
            .await,
    )?;
    let items = match selected {
        Outcome::Applied(items) => items,
        Outcome::Rejected(reason) => return Err(failure(reason)),
    };
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({"repository_id":repo.id,"items":items})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    repository_id: uuid::Uuid,
    request_id: String,
    items: Vec<bulk::Selected>,
    action: Action,
}
async fn edit(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key).map_err(IntoResponse::into_response)?;
    let Json(input) = input
        .map_err(Error::from)
        .map_err(IntoResponse::into_response)?;
    if input.repository_id != repo.id {
        return Err(Error::Invalid(
            "Repository identity changed; recover the original action from its repository",
        )
        .into_response());
    }
    let actor = app::actor(&principal).map_err(IntoResponse::into_response)?;
    let submission_id =
        app::submission_id(&input.request_id).map_err(IntoResponse::into_response)?;
    let (can_triage, eligible) = if let Action::Assignees { add, .. } = &input.action {
        let roster = Roster::load(&server, &principal, &repo)
            .await
            .map_err(IntoResponse::into_response)?;
        let mut eligible: Vec<_> = roster
            .items
            .into_iter()
            .filter(|person| add.contains(&person.id))
            .map(|person| Assignee {
                id: person.id,
                name: person.name,
            })
            .collect();
        eligible.sort_unstable_by_key(|person| person.id);
        (roster.principal.can_triage(repo.id), eligible)
    } else {
        (
            app::membership(&server, &principal, &repo)
                .await
                .map_err(IntoResponse::into_response)?
                .principal
                .can_triage(repo.id),
            vec![],
        )
    };
    let routed = app::repository_cell(&server, &repo).map_err(IntoResponse::into_response)?;
    let identity = app::mutation_identity().map_err(IntoResponse::into_response)?;
    let output = app::command_output(
        routed
            .client
            .command::<bulk::Edit>(
                &routed.target,
                identity,
                bulk::Input {
                    submission_id,
                    actor: (&actor).into(),
                    can_triage,
                    items: input.items,
                    action: input.action,
                    eligible,
                },
            )
            .await,
    )
    .map_err(IntoResponse::into_response)?;
    let count = match output {
        Outcome::Applied(count) => count,
        Outcome::Rejected(Failure::RequestConflict) => {
            return Err(Error::RequestConflict.into_response());
        }
        Outcome::Rejected(reason) => return Err(rejected(failure(reason))),
    };
    app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(json!({"count":count})))
}
fn failure(reason: Failure) -> Error {
    match reason {
        Failure::Invalid => {
            Error::Invalid("Select 1–100 distinct, versioned discussions and a valid change")
        }
        Failure::Archived => Error::Archived,
        Failure::Forbidden => Error::Forbidden,
        Failure::NotFound => Error::NotFound,
        Failure::Conflict => Error::Conflict,
        Failure::RequestConflict => Error::RequestConflict,
        Failure::MergePending => Error::MergePending,
        Failure::InvalidState => {
            Error::Invalid("The selection contains a discussion that cannot enter that state")
        }
        Failure::InvalidLabels => {
            Error::Invalid("Labels must exist and each item supports at most 20 labels")
        }
        Failure::InvalidAssignees => Error::Invalid(
            "Add current repository members; each item supports at most 10 assignees",
        ),
    }
}

#[cfg(test)]
mod tests;
