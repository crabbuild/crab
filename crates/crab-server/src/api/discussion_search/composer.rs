use std::{collections::BTreeSet, sync::Arc};

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    response::Redirect,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::members::roster,
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{ActorSummary, LookupActors},
        repository::{
            collaboration::{SubjectKind, SubjectSummary},
            search::index::references::{FindReferences, ReferenceInput},
        },
    },
    repository::Repository,
    server::Server,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    repository_id: Uuid,
    #[serde(default)]
    search: String,
}

#[derive(Serialize)]
pub(super) struct Suggestions<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReferenceParameters {
    repository_id: Option<Uuid>,
    #[serde(default)]
    search: String,
    #[serde(default)]
    exact: bool,
}

#[derive(Serialize)]
pub(super) struct ReferenceSuggestions {
    repository_id: Uuid,
    items: Vec<SubjectSummary>,
}

async fn find_references(
    server: &Server,
    repo: &Repository,
    input: ReferenceInput,
) -> Result<Vec<SubjectSummary>> {
    input.validate().map_err(Error::Invalid)?;
    let cell = app::repository_cell(server, repo)?;
    app::query_output(
        cell.client
            .query::<FindReferences>(&cell.target, None, input)
            .await,
    )
}

pub(super) async fn references(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<ReferenceParameters>,
) -> Result<Json<ReferenceSuggestions>> {
    let repo = app::repository(&server, &principal, &key)?;
    if params.repository_id.is_some_and(|id| repo.id != id) {
        return Err(Error::NotFound);
    }
    let items = find_references(
        &server,
        &repo,
        ReferenceInput {
            search: params.search,
            exact: params.exact,
        },
    )
    .await?;
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(ReferenceSuggestions {
        repository_id: repo.id,
        items,
    }))
}

pub(super) async fn open_reference(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, kind, number)): Path<(Uuid, SubjectKind, u64)>,
) -> Result<Redirect> {
    let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
    app::membership(&server, &principal, &repo).await?;
    let number = app::number(number)?;
    let items = find_references(
        &server,
        &repo,
        ReferenceInput {
            search: number.to_string(),
            exact: true,
        },
    )
    .await?;
    if !items.iter().any(|item| item.subject.kind == kind) {
        return Err(Error::NotFound);
    }
    app::membership(&server, &principal, &repo).await?;
    let path = match kind {
        SubjectKind::Issue => "issues",
        SubjectKind::Pull => "pull",
    };
    // Catalog owner and name contain only URL-safe characters. Keep the UUID
    // on the destination so the browser can reject a reused repository name.
    Ok(Redirect::to(&format!(
        "/{}/{}/{path}/{number}?repository_id={id}",
        repo.config.owner, repo.config.name,
    )))
}

pub(super) async fn mentions(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(params): Query<Parameters>,
) -> Result<Json<Suggestions<ActorSummary>>> {
    let repo = app::repository(&server, &principal, &key)?;
    if repo.id != params.repository_id {
        return Err(Error::NotFound);
    }
    let input = ReferenceInput {
        search: params.search,
        exact: false,
    };
    input.validate().map_err(Error::Invalid)?;
    let search = input.search.trim().to_lowercase();
    let initial = app::membership(&server, &principal, &repo).await?;
    let keys = roster::identities(&principal, &roster::members(&initial))?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let mut matches = Vec::new();
    for batch in keys.chunks(100) {
        let profiles = app::query_output(
            cells
                .query::<LookupActors>(Address::Directory, None, batch.to_vec())
                .await,
        )?;
        if profiles.len() != batch.len() {
            return Err(Error::CellContract(
                "Mention lookup returned an invalid result count",
            ));
        }
        matches.extend(batch.iter().zip(profiles).filter_map(|(key, profile)| {
            let profile = profile?;
            (profile.handle.contains(&search) || profile.name.to_lowercase().contains(&search))
                .then_some((&key.subject, profile))
        }));
    }
    let current = app::membership(&server, &principal, &repo).await?;
    let current: BTreeSet<_> = roster::members(&current)
        .into_iter()
        .map(|member| member.subject)
        .collect();
    matches.retain(|(subject, _)| current.contains(*subject));
    matches.sort_unstable_by(|(_, left), (_, right)| {
        (!left.handle.starts_with(&search), &left.handle)
            .cmp(&(!right.handle.starts_with(&search), &right.handle))
    });
    let items = matches
        .into_iter()
        .take(12)
        .map(|(_, profile)| profile)
        .collect();
    Ok(Json(Suggestions { items }))
}

#[cfg(test)]
mod tests;
