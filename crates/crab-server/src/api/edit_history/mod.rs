use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::{RepositoryAuthor, collaboration::edit_history as history},
    server::Server,
};

use super::{discussion_target::target, profiles::attribution::Authors};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/repos/{owner}/{name}/edit-history/{*target}",
            get(read).delete(redact),
        )
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    before: Option<u64>,
    revision: Option<u64>,
}

fn revision_view(authors: &Authors, revision: &history::Revision) -> Value {
    json!({
        "number": revision.number,
        "editor": authors.view(&revision.editor),
        "edited_at": revision.edited_at,
        "redaction": revision.redaction.as_ref().map(|removal| json!({
            "author": authors.view(&removal.author), "at": removal.at,
        })),
    })
}

fn authors(revision: &history::Revision) -> impl Iterator<Item = &RepositoryAuthor> {
    std::iter::once(&revision.editor).chain(revision.redaction.as_ref().map(|value| &value.author))
}

async fn read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, path)): Path<(String, String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let target = target(&path)?;
    let cell = app::repository_cell(&server, &repo)?;
    let actor = app::actor(&principal)?;
    let can_redact = |author: &RepositoryAuthor, archived: bool, writes: bool| {
        !archived && (writes || author.issuer == actor.issuer && author.subject == actor.subject)
    };
    if let Some(number) = params.revision {
        if params.before.is_some() {
            return Err(Error::Invalid(
                "Choose either a revision or a history cursor",
            ));
        }
        let number = app::number(number)?;
        let detail = app::query_output(
            cell.client
                .query::<history::Get>(&cell.target, None, history::Key { target, number })
                .await,
        )?
        .ok_or(Error::NotFound)?;
        let profiles = Authors::load(&server, &principal, authors(&detail.revision)).await?;
        let membership = app::membership(&server, &principal, &repo).await?;
        return Ok(Json(json!({
            "revision": revision_view(&profiles, &detail.revision),
            "before": detail.before,
            "after": detail.after,
            "can_redact": can_redact(&detail.author, detail.archived, membership.principal.can_write(repo.id)),
        })));
    }
    let before = params.before.map(app::number).transpose()?;
    let page = app::query_output(
        cell.client
            .query::<history::List>(&cell.target, None, history::Query { target, before })
            .await,
    )?
    .ok_or(Error::NotFound)?;
    let profiles = Authors::load(&server, &principal, page.items.iter().flat_map(authors)).await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(json!({
        "items": page.items.iter().map(|item| revision_view(&profiles, item)).collect::<Vec<_>>(),
        "next": page.next,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    revision: u64,
}

async fn redact(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((owner, name, path)): Path<(String, String, String)>,
    Query(params): Query<Selection>,
) -> Result<Json<Value>> {
    let repo = app::repository(&server, &principal, &(owner, name))?;
    let key = history::Key {
        target: target(&path)?,
        number: app::number(params.revision)?,
    };
    let actor = app::actor(&principal)?;
    let cell = app::repository_cell(&server, &repo)?;
    let membership = app::membership(&server, &principal, &repo).await?;
    let outcome = app::command_output(
        cell.client
            .command::<history::Redact>(
                &cell.target,
                app::mutation_identity()?,
                history::Removal {
                    key,
                    actor: (&actor).into(),
                    can_write: membership.principal.can_write(repo.id),
                },
            )
            .await,
    )?;
    match outcome {
        history::Outcome::Redacted => {
            app::membership(&server, &principal, &repo).await?;
            Ok(Json(json!({"redacted": true})))
        }
        history::Outcome::Archived => Err(Error::Archived),
        history::Outcome::NotFound => Err(Error::NotFound),
        history::Outcome::Forbidden => Err(Error::EditHistoryPermission),
    }
}

#[cfg(test)]
mod tests;
