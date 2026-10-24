use crate::{
    api::{profiles::attribution::Authors, write::rejected},
    app::{self, Error},
    auth::Principal,
    cells::repository::issues::pins::{self, GetPins, Outcome, SetPins},
    server::Server,
};
use axum::{
    Extension, Json,
    extract::{Path, State, rejection::JsonRejection},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> app::Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &key)?;
    let routed = app::repository_cell(&server, &repo)?;
    let page = app::query_output(
        routed
            .client
            .query::<GetPins>(&routed.target, None, ())
            .await,
    )?;
    let authors = Authors::load(
        &server,
        &principal,
        page.items.iter().map(|item| &item.author),
    )
    .await?;
    let current = app::membership(&server, &principal, &repo).await?;
    let items = page.items.iter().map(|item| json!({"number":item.number,"title":item.title,"state":if item.state==0 {"open"} else {"closed"},"state_reason":item.state_reason,"author":authors.view(&item.author),"created_at":item.created_at,"comment_count":item.comments})).collect::<Vec<_>>();
    Ok(Json(
        json!({"version":page.version,"items":items,"can_manage":current.principal.can_write(repo.id) && !page.archived}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    request_id: String,
    version: u64,
    numbers: Vec<u64>,
}
pub(super) async fn set(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    input: std::result::Result<Json<Input>, JsonRejection>,
) -> std::result::Result<Json<Value>, Response> {
    let repo = app::repository(&server, &principal, &key).map_err(IntoResponse::into_response)?;
    let Json(input) = input
        .map_err(Error::from)
        .map_err(IntoResponse::into_response)?;
    let actor = app::actor(&principal).map_err(IntoResponse::into_response)?;
    let submission_id =
        app::submission_id(&input.request_id).map_err(IntoResponse::into_response)?;
    let current = app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    let routed = app::repository_cell(&server, &repo).map_err(IntoResponse::into_response)?;
    let identity = app::mutation_identity().map_err(IntoResponse::into_response)?;
    let result = app::command_output(
        routed
            .client
            .command::<SetPins>(
                &routed.target,
                identity,
                pins::Input {
                    submission_id,
                    actor: (&actor).into(),
                    can_write: current.principal.can_write(repo.id),
                    version: input.version,
                    numbers: input.numbers,
                },
            )
            .await,
    )
    .map_err(IntoResponse::into_response)?;
    let version = match result {
        Outcome::Applied(version) => version,
        Outcome::RequestConflict => return Err(Error::RequestConflict.into_response()),
        Outcome::Invalid => return Err(rejected(Error::Invalid("Select distinct valid issues"))),
        Outcome::Limit => {
            return Err(rejected(Error::Invalid(
                "A repository can pin up to three issues",
            )));
        }
        Outcome::Archived => return Err(rejected(Error::Archived)),
        Outcome::Forbidden => return Err(rejected(Error::PinPermission)),
        Outcome::NotFound => return Err(rejected(Error::NotFound)),
        Outcome::Conflict => return Err(rejected(Error::Conflict)),
    };
    app::membership(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(Json(json!({"version":version})))
}

#[cfg(test)]
mod tests;
