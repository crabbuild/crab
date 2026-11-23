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
mod scan;

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
    fork: bool,
}

#[derive(Default)]
struct Window<'a> {
    entries: Vec<&'a Arc<Repository>>,
    boundary: Option<&'a Arc<Repository>>,
    property_revision: Option<u64>,
    archive_versions: HashMap<Uuid, u64>,
    invalid_property: bool,
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
    let filtered = query.filtered();
    let property_filter = !query.terms.is_empty();
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
        if query
            .fork
            .is_none_or(|fork| repo.forked_from.is_some() == fork)
            && let Some(priority) = rank(repo, &query.text, &words)
        {
            ranks.insert(repo.id, priority);
            return true;
        }
        property_filter
    })
    .await?;
    let property_organization = search
        .organization
        .filter(|_| property_filter && !scoped.is_empty());
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
    let window = scan::window(
        server,
        &query,
        property_organization,
        &visible,
        start,
        limit,
    )
    .await?;
    let page = &window.entries[..limit.min(window.entries.len())];
    let checked: Vec<_> = window
        .entries
        .iter()
        .copied()
        .chain(window.boundary)
        .chain(start.checked_sub(1).and_then(|index| visible.get(index)))
        .collect();
    let archive_targets = if query.archived.is_some() {
        &checked
    } else {
        page
    };
    let archive_repositories: Vec<_> = archive_targets.iter().copied().cloned().collect();
    let archives = scan::lifecycles(server, archive_repositories).await?;
    if archive_targets
        .iter()
        .zip(&archives)
        .any(|(repo, lifecycle)| {
            window
                .archive_versions
                .get(&repo.id)
                .is_some_and(|expected| *expected != lifecycle.version)
        })
    {
        return Err(Error::PageChanged);
    }
    if let (Some(organization), Some(revision)) = (property_organization, window.property_revision)
        && properties::revision(server, organization).await? != revision
    {
        return Err(Error::PageChanged);
    }
    let scope = filtered.then(|| scan::scope(server, &visible, window.property_revision));
    let cursor_state = |id| -> Result<Option<blake3::Hash>> {
        let Some(mut scope) = scope.clone() else {
            return Ok(None);
        };
        if query.archived.is_some() {
            let version = archive_targets
                .iter()
                .zip(&archives)
                .find_map(|(repo, record)| (repo.id == id).then_some(record.version))
                .ok_or(Error::CellContract(
                    "Repository search archive boundary is missing",
                ))?;
            scope.update(&version.to_le_bytes());
        }
        Ok(Some(scope.finalize()))
    };
    if let Some(cursor) = &after
        && cursor.state != cursor_state(cursor.id)?
    {
        return Err(Error::PageChanged);
    }
    if window.invalid_property {
        return Err(Error::Invalid(
            "Choose an existing repository property and a valid value",
        ));
    }
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
                    && fresh.forked_from == repo.forked_from
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
        .map(|repo| -> Result<String> {
            Ok(Cursor {
                id: repo.id,
                state: cursor_state(repo.id)?,
            }
            .encode(&key))
        })
        .transpose()?;
    let repositories: Vec<_> = page
        .iter()
        .zip(archives)
        .map(|(repo, lifecycle)| Entry {
            id: repo.id,
            owner: &repo.config.owner,
            name: &repo.config.name,
            description: &repo.config.description,
            archived: lifecycle.archived,
            fork: repo.forked_from.is_some(),
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
