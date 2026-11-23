use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::Json;
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

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
    let query = search
        .query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let signature = |id: Uuid| {
        let mut digest = blake3::Hasher::new_keyed(&server.cursor_key);
        digest.update(b"crab-server.repository-browse.v1");
        for value in [
            actor.issuer.as_bytes(),
            actor.subject.as_bytes(),
            query.as_bytes(),
        ] {
            digest.update(&(value.len() as u64).to_le_bytes());
            digest.update(value);
        }
        digest.update(search.organization.unwrap_or_default().as_bytes());
        digest.update(id.as_bytes());
        digest.finalize().to_hex()
    };
    let after = search
        .after
        .as_deref()
        .map(|value| {
            if value.len() != 97 {
                return Err(Error::Invalid("Invalid repository search cursor"));
            }
            let (id, tag) = value
                .split_once('.')
                .ok_or(Error::Invalid("Invalid repository search cursor"))?;
            let id = Uuid::parse_str(id)
                .map_err(|_| Error::Invalid("Invalid repository search cursor"))?;
            if signature(id).as_str() != tag {
                return Err(Error::Invalid("Invalid repository search cursor"));
            }
            Ok(id)
        })
        .transpose()?;
    let words: Vec<_> = query.split_whitespace().collect();
    let mut matches = HashMap::new();
    let mut visible: Vec<_> = visible_matching(server, principal, |repo| {
        if search
            .organization
            .is_some_and(|id| repo.organization != Some(id))
        {
            return false;
        }
        let Some(priority) = rank(repo, &query, &words) else {
            return false;
        };
        matches.insert(repo.id, priority);
        true
    })
    .await?
    .into_iter()
    .map(|row| row.repository)
    .collect();
    visible.sort_by(|a, b| {
        (matches.get(&a.id), &a.config.owner, &a.config.name).cmp(&(
            matches.get(&b.id),
            &b.config.owner,
            &b.config.name,
        ))
    });
    let start = after.map_or(Ok(0), |id| {
        visible
            .iter()
            .position(|repo| repo.id == id)
            .map(|index| index + 1)
            .ok_or(Error::PageChanged)
    })?;
    let window = &visible[start..(start + limit + 1).min(visible.len())];
    let page = &window[..limit.min(window.len())];
    let archives: Vec<_> = stream::iter(page.iter().cloned())
        .map(|repo: Arc<Repository>| async move {
            settings::load_lifecycle(server, &repo)
                .await
                .map(|state| state.archived)
        })
        .buffered(8)
        .try_collect()
        .await?;
    let ids: HashSet<_> = window.iter().map(|repo| repo.id).chain(after).collect();
    // An organization grant or direct membership may change while we read the
    // page's archive state. Recheck even the lookahead used to issue a cursor.
    let current = visible_matching(server, principal, |repo| ids.contains(&repo.id)).await?;
    if current.len() != ids.len()
        || window.iter().any(|repo| {
            !current.iter().any(|row| {
                let fresh = &row.repository;
                fresh.id == repo.id
                    && fresh.config.owner == repo.config.owner
                    && fresh.config.name == repo.config.name
                    && fresh.config.description == repo.config.description
            })
        })
    {
        return Err(Error::PageChanged);
    }
    let next = page
        .last()
        .filter(|_| window.len() > limit)
        .map(|repo| format!("{}.{}", repo.id.simple(), signature(repo.id)));
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
