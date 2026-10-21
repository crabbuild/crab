use axum::{
    Extension, Json,
    extract::{Path, Query as QueryParameters, State},
};
use std::sync::Arc;

use crate::{
    app::{self, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, CompleteHandles, ResolveHandles, validate_handle_prefix},
        repository::{
            SearchInput,
            search::{
                Choice, Query,
                authors::{self, Binding},
            },
        },
    },
    server::Server,
};

pub(in crate::api) async fn resolve(
    server: &Server,
    principal: &Principal,
    input: &mut SearchInput,
) -> Result<()> {
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    let names = query.authors();
    if names.is_empty() {
        input.authors.clear();
        return Ok(());
    }
    let handles: Vec<_> = names
        .iter()
        .filter(|name| name.as_str() != "@me")
        .cloned()
        .collect();
    let resolved = if handles.is_empty() {
        Vec::new()
    } else {
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        app::query_output(
            cells
                .query::<ResolveHandles>(Address::Directory, None, handles.clone())
                .await,
        )?
    };
    if resolved.len() != handles.len() {
        return Err(app::Error::CellContract(
            "Author resolution returned an invalid result count",
        ));
    }
    let mut by_handle: std::collections::BTreeMap<_, _> =
        handles.into_iter().zip(resolved).collect();
    let actor = app::actor(principal)?;
    input.authors = names
        .into_iter()
        .map(|name| {
            let identity = if name == "@me" {
                Some(authors::identity(&actor.issuer, &actor.subject).map_err(app::Error::Cell)?)
            } else {
                by_handle
                    .remove(&name)
                    .flatten()
                    .map(|key| authors::identity(&key.issuer, &key.subject))
                    .transpose()
                    .map_err(app::Error::Cell)?
            };
            Ok(Binding { name, identity })
        })
        .collect::<Result<_>>()?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct OptionInput {
    query: Option<String>,
    search: String,
}

#[derive(serde::Serialize)]
struct AuthorOption {
    actor: ActorSummary,
    #[serde(flatten)]
    choice: Choice,
}

#[derive(serde::Serialize)]
pub(super) struct Options {
    items: Vec<AuthorOption>,
    clear: Choice,
    #[serde(rename = "self")]
    current: Choice,
    truncated: bool,
}

pub(super) async fn options(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    QueryParameters(input): QueryParameters<OptionInput>,
) -> Result<Json<Options>> {
    let repo = app::repository(&server, &principal, &key)?;
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    if input.search.chars().count() > 256 || input.search.chars().any(char::is_control) {
        return Err(app::Error::Invalid(
            "Author search must contain at most 256 characters without controls",
        ));
    }
    let search = input.search.trim().to_ascii_lowercase();
    let prefix = search.strip_prefix('@').unwrap_or(&search);
    let mut items = Vec::new();
    let mut truncated = false;
    if validate_handle_prefix(prefix).is_ok() {
        let cells = server.cells().ok_or(app::Error::CellUnavailable)?;
        let matches = app::query_output(
            cells
                .query::<CompleteHandles>(Address::Directory, None, prefix.to_owned())
                .await,
        )?;
        truncated = matches.truncated;
        for actor in matches.items {
            items.push(AuthorOption {
                choice: query
                    .author_choice(Some(&actor.handle))
                    .map_err(app::Error::Invalid)?,
                actor,
            });
        }
    }
    // Directory names are public choices, never grants. Recheck repository
    // access after the cross-authority read before returning its filter menu.
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(Options {
        items,
        clear: query.author_choice(None).map_err(app::Error::Invalid)?,
        current: query
            .author_choice(Some("@me"))
            .map_err(app::Error::Invalid)?,
        truncated,
    }))
}
