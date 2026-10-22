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
                people::{self, Binding},
            },
        },
    },
    repository::Repository,
    server::Server,
};
use axum::Json;
pub(in crate::api) async fn resolve(
    server: &Server,
    principal: &Principal,
    input: &mut SearchInput,
) -> Result<()> {
    let query = Query::parse(input.query.as_deref()).map_err(app::Error::Invalid)?;
    let names: std::collections::BTreeSet<_> =
        query.authors().union(&query.reviewers()).cloned().collect();
    if names.is_empty() {
        input.authors.clear();
        input.reviewers.clear();
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
            "People resolution returned an invalid result count",
        ));
    }
    let mut by_handle: std::collections::BTreeMap<_, _> =
        handles.into_iter().zip(resolved).collect();
    let actor = app::actor(principal)?;
    let resolved = names
        .into_iter()
        .map(|name| {
            let identity = if name == "@me" {
                Some(people::identity(&actor.issuer, &actor.subject).map_err(app::Error::Cell)?)
            } else {
                by_handle
                    .remove(&name)
                    .flatten()
                    .map(|key| people::identity(&key.issuer, &key.subject))
                    .transpose()
                    .map_err(app::Error::Cell)?
            };
            Ok((name, identity))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
    // Both families bind against the same parsed union and Directory snapshot.
    let bind = |name: String| Binding {
        identity: resolved[&name],
        name,
    };
    input.authors = query.authors().into_iter().map(bind).collect();
    input.reviewers = query.reviewers().into_iter().map(bind).collect();
    Ok(())
}

#[derive(serde::Serialize)]
struct PersonOption {
    actor: ActorSummary,
    #[serde(flatten)]
    choice: Choice,
}

#[derive(serde::Serialize)]
pub(super) struct Options {
    items: Vec<PersonOption>,
    clear: Choice,
    #[serde(rename = "self")]
    current: Choice,
    truncated: bool,
}

pub(super) async fn options(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    query: &Query,
    search: &str,
    choose: impl Fn(&Query, Option<&str>) -> std::result::Result<Choice, &'static str>,
) -> Result<Json<Options>> {
    if search.chars().count() > 256 || search.chars().any(char::is_control) {
        return Err(app::Error::Invalid(
            "People search must contain at most 256 characters without controls",
        ));
    }
    let search = search.trim().to_ascii_lowercase();
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
            items.push(PersonOption {
                choice: choose(query, Some(&actor.handle)).map_err(app::Error::Invalid)?,
                actor,
            });
        }
    }
    // Directory names are public choices, never grants. Recheck repository
    // access after the cross-authority read before returning its filter menu.
    app::membership(server, principal, repo).await?;
    Ok(Json(Options {
        items,
        clear: choose(query, None).map_err(app::Error::Invalid)?,
        current: choose(query, Some("@me")).map_err(app::Error::Invalid)?,
        truncated,
    }))
}
