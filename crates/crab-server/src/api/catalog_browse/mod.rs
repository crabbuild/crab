use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::Json;
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

mod properties;
mod query;

use query::{Cursor, Query};

use super::catalog::visible_matching;
use crate::{
    app::{Error, Result},
    auth::{Identity, Principal},
    repository::{Repository, settings},
    server::Server,
};

pub(super) struct Search {
    pub organization: Option<Uuid>,
    pub query: String,
    pub after: Option<String>,
    pub limit: Option<u8>,
}

#[derive(Serialize)]
struct Entry<'a> {
    id: Uuid,
    owner: &'a str,
    name: &'a str,
    description: &'a str,
    archived: bool,
}

#[derive(Default)]
struct Window<'a> {
    entries: Vec<&'a Arc<Repository>>,
    boundary: Option<&'a Arc<Repository>>,
    property_state: Option<(blake3::Hash, u64)>,
}

pub(super) async fn list(
    server: &Server,
    principal: &Principal,
    actor: &Identity,
    search: Search,
) -> Result<Json<Value>> {
    let limit = usize::from(search.limit.unwrap_or(32));
    if !(1..=50).contains(&limit) || search.query.chars().count() > 200 {
        return Err(Error::Invalid(
            "Repository search accepts at most 200 characters and pages of 1–50",
        ));
    }
    let query = Query::parse(&search.query, search.organization)?;
    let filtered = !query.terms.is_empty();
    let key = query.cursor_key(server, actor, search.organization);
    let after = search
        .after
        .as_deref()
        .map(|value| Cursor::decode(value, &key, filtered))
        .transpose()?;
    let words: Vec<_> = query.text.split_whitespace().collect();
    let mut ranks = HashMap::new();
    let scoped = visible_matching(server, principal, |repo| {
        if search
            .organization
            .is_some_and(|id| repo.organization != Some(id))
        {
            return false;
        }
        if let Some(priority) = rank(repo, &query.text, &words) {
            ranks.insert(repo.id, priority);
            return true;
        }
        filtered
    })
    .await?;
    let can_filter = filtered && !scoped.is_empty();
    let mut ranked: Vec<_> = scoped
        .into_iter()
        .filter_map(|row| {
            ranks
                .remove(&row.repository.id)
                .map(|rank| (rank, row.repository))
        })
        .collect();
    ranked.sort_by(|(rank_a, a), (rank_b, b)| {
        (rank_a, &a.config.owner, &a.config.name).cmp(&(rank_b, &b.config.owner, &b.config.name))
    });
    let visible: Vec<_> = ranked.into_iter().map(|(_, repo)| repo).collect();
    let start = after.as_ref().map_or(Ok(0), |cursor| {
        visible
            .iter()
            .position(|repo| repo.id == cursor.id)
            .map(|index| index + 1)
            .ok_or(Error::PageChanged)
    })?;
    let window = if let Some(organization) = search.organization.filter(|_| can_filter) {
        properties::scan(
            server,
            organization,
            &query.terms,
            &visible,
            start,
            limit,
            after.as_ref().and_then(|cursor| cursor.state),
        )
        .await?
    } else {
        Window {
            entries: visible[start..(start + limit + 1).min(visible.len())]
                .iter()
                .collect(),
            ..Window::default()
        }
    };
    let page = &window.entries[..limit.min(window.entries.len())];
    let archive_repositories: Vec<_> = page.iter().copied().cloned().collect();
    let archives: Vec<_> = stream::iter(archive_repositories)
        .map(|repo: Arc<Repository>| async move {
            settings::load_lifecycle(server, &repo)
                .await
                .map(|state| state.archived)
        })
        .buffered(8)
        .try_collect()
        .await?;
    if let (Some(organization), Some((_, revision))) = (search.organization, window.property_state)
        && properties::revision(server, organization).await? != revision
    {
        return Err(Error::PageChanged);
    }
    let checked: Vec<_> = window
        .entries
        .iter()
        .copied()
        .chain(window.boundary)
        .chain(start.checked_sub(1).and_then(|index| visible.get(index)))
        .collect();
    let ids: HashSet<_> = checked.iter().map(|repo| repo.id).collect();
    // An organization grant or direct membership may change while we read the
    // page's archive state. Recheck even the lookahead used to issue a cursor.
    let current = visible_matching(server, principal, |repo| ids.contains(&repo.id)).await?;
    if current.len() != ids.len()
        || checked.iter().any(|repo| {
            !current.iter().any(|row| {
                let fresh = &row.repository;
                fresh.id == repo.id
                    && fresh.config.owner == repo.config.owner
                    && fresh.config.name == repo.config.name
                    && fresh.config.description == repo.config.description
                    && fresh.organization == repo.organization
                    && fresh.ownership_epoch == repo.ownership_epoch
            })
        })
    {
        return Err(Error::PageChanged);
    }
    let next = page
        .last()
        .copied()
        .filter(|_| window.entries.len() > limit)
        .or(window.boundary)
        .map(|repo| {
            Cursor {
                id: repo.id,
                state: window.property_state.map(|(state, _)| state),
            }
            .encode(&key)
        });
    let repositories: Vec<_> = page
        .iter()
        .zip(archives)
        .map(|(repo, archived)| Entry {
            id: repo.id,
            owner: &repo.config.owner,
            name: &repo.config.name,
            description: &repo.config.description,
            archived,
        })
        .collect();
    Ok(Json(
        serde_json::json!({"repositories": repositories, "next": next}),
    ))
}

fn rank(repo: &Repository, query: &str, words: &[&str]) -> Option<u8> {
    if words.is_empty() {
        return Some(0);
    }
    let name = repo.config.name.to_ascii_lowercase();
    let qualified = format!("{}/{}", repo.config.owner.to_ascii_lowercase(), name);
    if name == query || qualified == query {
        return Some(0);
    }
    if name.starts_with(query) || qualified.starts_with(query) {
        return Some(1);
    }
    if words.iter().all(|word| qualified.contains(word)) {
        return Some(2);
    }
    let description = repo.config.description.to_lowercase();
    words
        .iter()
        .all(|word| qualified.contains(word) || description.contains(word))
        .then_some(3)
}

#[cfg(test)]
mod tests;
