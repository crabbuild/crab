use super::discussion_target::target;
use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    middleware,
    routing::put,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::{self, Error, Result},
    auth::{Identity, Principal},
    cells::repository::collaboration::Target,
    cells::repository::collaboration::reactions::{
        GetReactions, Group, Kind, Outcome, Query, Selection, SetReaction,
    },
    repository::Repository,
    server::Server,
};

#[derive(Serialize)]
pub(super) struct View {
    #[serde(skip)]
    pub(super) minimization: Option<crate::cells::repository::collaboration::minimization::State>,
    #[serde(skip)]
    pub(super) has_edit_history: bool,
    revision: u64,
    groups: BTreeMap<Kind, Group>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesiredReaction {
    kind: Kind,
    active: bool,
}

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/repos/{owner}/{name}/reactions/{*target}", put(select))
        .layer(axum::extract::DefaultBodyLimit::max(2048))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

async fn select(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, path)): Path<(String, String, String)>,
    input: std::result::Result<Json<DesiredReaction>, JsonRejection>,
) -> Result<Json<View>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let actor = app::actor(&principal)?;
    let Json(input) = input?;
    let cell = app::repository_cell(&server, &repo)?;
    let result = cell
        .client
        .command::<SetReaction>(
            &cell.target,
            app::mutation_identity()?,
            Selection {
                target: target(&path)?,
                kind: input.kind,
                active: input.active,
                actor: (&actor).into(),
            },
        )
        .await
        .map_err(Error::Cell)?;
    let summary = match result.output {
        Outcome::Applied(value) => value,
        Outcome::Archived => return Err(Error::Archived),
        Outcome::Locked => return Err(Error::ConversationLocked),
        Outcome::NotFound => return Err(Error::NotFound),
    };
    Ok(Json(View {
        revision: result.receipt.revision,
        groups: summary.groups,
        minimization: None,
        has_edit_history: false,
    }))
}

pub(super) async fn read(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    targets: Vec<Target>,
) -> Result<Vec<View>> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let count = targets.len();
    let cell = app::repository_cell(server, repo)?;
    let observed = cell
        .client
        .query::<GetReactions>(
            &cell.target,
            None,
            Query {
                targets,
                actor: actor.into(),
            },
        )
        .await
        .map_err(Error::Cell)?;
    if observed.output.len() != count {
        return Err(Error::Cell(crate::cells::Error::Invalid(
            "reaction query result length is invalid",
        )));
    }
    // The Cell receipt orders refreshes and completed writes, including after
    // the last reaction is removed. No per-target tombstone is required.
    observed
        .output
        .into_iter()
        .map(|value| {
            let value = value.ok_or(Error::NotFound)?;
            Ok(View {
                revision: observed.receipt.revision,
                groups: value.groups,
                minimization: value.minimization,
                has_edit_history: value.has_edit_history,
            })
        })
        .collect()
}

pub(super) async fn one(
    server: &Server,
    repo: &Repository,
    actor: &Identity,
    target: Target,
) -> Result<View> {
    read(server, repo, actor, vec![target])
        .await?
        .pop()
        .ok_or(Error::NotFound)
}

#[cfg(test)]
mod tests;
