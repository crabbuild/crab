use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use super::{RepositoryLabel, subscriptions};
use crate::{
    api::catalog,
    app::{self, Error, Result},
    auth::Principal,
    cells::repository::collaboration::watches::{Mode, Preference},
    config::valid_repository_component,
    repository::Repository,
    server::Server,
};

pub(super) mod bulk;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    owner: Option<String>,
    after: Option<String>,
    limit: Option<u8>,
}

#[derive(Serialize)]
struct Item {
    repository: RepositoryLabel,
    preference: Preference,
}

#[derive(Serialize)]
pub(super) struct Page {
    items: Vec<Item>,
    next: Option<String>,
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> Result<Json<Page>> {
    let actor = app::account(&principal)?;
    let limit = usize::from(params.limit.unwrap_or(32));
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid(
            "Watched repository pages require a limit of 1–50",
        ));
    }
    let owner = params.owner.as_deref().map(valid_owner).transpose()?;
    let after = params.after.as_deref().map(cursor).transpose()?;
    if owner.is_some_and(|owner| after.is_some_and(|(previous, _)| owner != previous)) {
        return Err(Error::Invalid(
            "Watched repository cursor belongs to another owner",
        ));
    }
    let visible = window(&server, &principal, owner, after, limit + 1).await?;
    let scanned = &visible[..limit.min(visible.len())];
    if scanned.is_empty() {
        return Ok(Json(Page {
            items: Vec::new(),
            next: None,
        }));
    }

    let preferences: Vec<_> = stream::iter(scanned.iter().cloned())
        .map(|repo| {
            let (server, actor) = (&server, actor.clone());
            async move { subscriptions::read(server, &repo, actor).await }
        })
        .buffered(8)
        .collect()
        .await;

    // Preference reads can run concurrently with permission changes. Refresh
    // grants before emitting even a cursor, since it names a scanned repository.
    let current = window(&server, &principal, owner, after, limit + 1).await?;
    if !scanned
        .iter()
        .map(|repo| (repo.id, key(repo)))
        .eq(current.iter().take(limit).map(|repo| (repo.id, key(repo))))
    {
        return Err(Error::PageChanged);
    }
    let mut items = Vec::new();
    for (repo, preference) in scanned.iter().zip(preferences) {
        let preference = preference?;
        if preference.mode == Mode::Muted
            || (preference.mode == Mode::Direct && !preference.releases)
        {
            continue;
        }
        items.push(Item {
            repository: RepositoryLabel {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
            },
            preference,
        });
    }
    let next = (current.len() > limit).then(|| {
        let (owner, name) = key(&current[limit - 1]);
        format!("{owner}/{name}")
    });
    Ok(Json(Page { items, next }))
}

async fn window(
    server: &Server,
    principal: &Principal,
    owner: Option<&str>,
    after: Option<(&str, &str)>,
    limit: usize,
) -> Result<Vec<Arc<Repository>>> {
    Ok(catalog::visible(server, principal, None, None)
        .await?
        .into_iter()
        .map(|visible| visible.repository)
        .filter(|repo| {
            owner.is_none_or(|owner| key(repo).0 == owner)
                && after.is_none_or(|after| key(repo) > after)
        })
        .take(limit)
        .collect())
}

fn key(repo: &Repository) -> (&str, &str) {
    (&repo.config.owner, &repo.config.name)
}

fn valid_owner(owner: &str) -> Result<&str> {
    valid_repository_component(owner)
        .then_some(owner)
        .ok_or(Error::Invalid("Watched repository owner is invalid"))
}

fn cursor(text: &str) -> Result<(&str, &str)> {
    text.split_once('/')
        .filter(|(owner, name)| {
            valid_repository_component(owner) && valid_repository_component(name)
        })
        .ok_or(Error::Invalid("Watched repository cursor is invalid"))
}

#[cfg(test)]
mod tests;
