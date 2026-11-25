use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::Json;
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

mod properties;
mod query;
mod scan;

use query::{Cursor, Order, Query};

use super::catalog::visible_matching;
use crate::{
    RepositoryAccess,
    app::{Error, Result},
    auth::Principal,
    catalog::visibility::Visibility,
    repository::{Repository, settings},
    server::Server,
};

pub(super) enum Scope {
    All,
    Public,
    Personal(String),
    Organization(Uuid),
    Forks(Uuid),
    Team {
        organization: Uuid,
        team: Uuid,
        ancestors: Vec<Uuid>,
    },
}

impl Scope {
    fn organization(&self) -> Option<Uuid> {
        match self {
            Self::Organization(id)
            | Self::Team {
                organization: id, ..
            } => Some(*id),
            _ => None,
        }
    }

    fn includes(&self, repo: &Repository) -> bool {
        match self {
            Self::All | Self::Public => true,
            Self::Personal(owner) => {
                repo.owning_organization().is_none() && repo.config.owner == *owner
            }
            Self::Forks(upstream) => repo.forked_from == Some(*upstream),
            Self::Organization(id)
            | Self::Team {
                organization: id, ..
            } => repo.owning_organization() == Some(*id),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Search {
    #[serde(default, rename = "q")]
    pub query: String,
    pub after: Option<String>,
    pub limit: Option<u8>,
}

#[derive(Serialize)]
struct Entry<'a> {
    id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata_updated_at_ms: Option<u64>,
    owner: &'a str,
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    topics: &'a [String],
    archived: bool,
    visibility: Visibility,
    fork: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    can_admin: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    team_access: Option<RepositoryAccess>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    team_access_inherited: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    can_manage_team_access: bool,
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
    scope: Scope,
    search: Search,
) -> Result<Json<Value>> {
    let limit = usize::from(search.limit.unwrap_or(32));
    if !(1..=50).contains(&limit) || search.query.chars().count() > 200 {
        return Err(Error::Invalid(
            "Repository search accepts at most 200 characters and pages of 1–50",
        ));
    }
    let actor = principal.identity();
    if actor.is_none() && !principal.permits_public_reads() {
        return Err(Error::SessionRequired);
    }
    let organization = scope.organization();
    let team = match &scope {
        Scope::Team {
            team, ancestors, ..
        } => Some((*team, ancestors.as_slice())),
        _ => None,
    };
    let query = Query::parse(&search.query, organization)?;
    let filtered = query.filtered();
    let property_filter = !query.terms.is_empty();
    if property_filter && actor.is_none() {
        return Err(Error::SessionRequired);
    }
    let member_principal;
    let principal = if property_filter {
        member_principal = principal.clone().without_public_reads();
        &member_principal
    } else {
        principal
    };
    let key = query.cursor_key(server, actor.as_ref(), &scope);
    let after = search
        .after
        .as_deref()
        .map(|value| Cursor::decode(value, &key, filtered))
        .transpose()?;
    let words: Vec<_> = query.text.split_whitespace().collect();
    let mut ranks = HashMap::new();
    let scoped = visible_matching(server, principal, team, |repo| {
        if !scope.includes(repo) {
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
    let property_organization = organization.filter(|_| property_filter && !scoped.is_empty());
    let mut topics = HashMap::new();
    let mut updates = HashMap::new();
    let mut visibility = HashMap::new();
    let mut team_grants = HashMap::new();
    let mut ranked: Vec<_> = scoped
        .into_iter()
        .filter_map(|row| {
            if matches!(scope, Scope::Public) && !row.visibility.is_public() {
                return None;
            }
            if query
                .visibility
                .is_some_and(|selected| selected != row.visibility)
            {
                return None;
            }
            let priority = ranks.remove(&row.repository.id)?;
            if !query.matches_topics(&row.topics) {
                return None;
            }
            topics.insert(row.repository.id, row.topics);
            if let Some(time) = row.metadata_updated_at_ms {
                updates.insert(row.repository.id, time);
            }
            visibility.insert(row.repository.id, row.visibility);
            if let Some(access) = row.team_access {
                team_grants.insert(
                    row.repository.id,
                    (access, row.organization_owner, row.team_access_inherited),
                );
            }
            Some((priority, row.repository))
        })
        .collect();
    let name = |repo: &Repository| {
        (
            repo.config.owner.to_ascii_lowercase(),
            repo.config.name.to_ascii_lowercase(),
        )
    };
    let updated = |repo: &Repository| updates.get(&repo.id).copied().or(repo.created_at_ms);
    let order = if matches!(scope, Scope::Forks(_))
        && words.is_empty()
        && query.order == Order::Relevance
    {
        Order::CreatedDesc
    } else {
        query.order
    };
    match order {
        Order::Relevance => ranked.sort_by(|(rank_a, a), (rank_b, b)| {
            (rank_a, &a.config.owner, &a.config.name).cmp(&(
                rank_b,
                &b.config.owner,
                &b.config.name,
            ))
        }),
        Order::NameAsc => ranked.sort_by_cached_key(|(_, repo)| name(repo)),
        Order::NameDesc => ranked.sort_by_cached_key(|(_, repo)| Reverse(name(repo))),
        Order::CreatedAsc => ranked.sort_by_cached_key(|(_, repo)| {
            (repo.created_at_ms.is_none(), repo.created_at_ms, name(repo))
        }),
        Order::CreatedDesc => {
            ranked.sort_by_cached_key(|(_, repo)| (Reverse(repo.created_at_ms), name(repo)))
        }
        Order::UpdatedAsc => ranked.sort_by_cached_key(|(_, repo)| {
            let date = updated(repo);
            (date.is_none(), date, name(repo))
        }),
        Order::UpdatedDesc => {
            ranked.sort_by_cached_key(|(_, repo)| (Reverse(updated(repo)), name(repo)))
        }
    }
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
    let snapshot_scope = filtered.then(|| {
        scan::scope(
            server,
            &visible,
            window.property_revision,
            query.order.by_update().then_some(&updates),
        )
    });
    let cursor_state = |id| -> Result<Option<blake3::Hash>> {
        let Some(mut scope) = snapshot_scope.clone() else {
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
    let current = visible_matching(server, principal, team, |repo| {
        ids.contains(&repo.id) && scope.includes(repo)
    })
    .await?;
    if current.len() != ids.len()
        || checked.iter().any(|repo| {
            !current.iter().any(|row| {
                let fresh = &row.repository;
                fresh.id == repo.id
                    && fresh.config.owner == repo.config.owner
                    && fresh.config.name == repo.config.name
                    && fresh.config.description == repo.config.description
                    && fresh.created_at_ms == repo.created_at_ms
                    && updates.get(&repo.id).copied() == row.metadata_updated_at_ms
                    && topics.get(&repo.id) == Some(&row.topics)
                    && visibility.get(&repo.id) == Some(&row.visibility)
                    && team_grants.get(&repo.id).copied()
                        == row.team_access.map(|access| {
                            (access, row.organization_owner, row.team_access_inherited)
                        })
                    && fresh.organization == repo.organization
                    && fresh.ownership_epoch == repo.ownership_epoch
                    && fresh.forked_from == repo.forked_from
            })
        })
    {
        return Err(Error::PageChanged);
    }
    let admins: HashSet<_> = current
        .iter()
        .filter(|row| row.principal.can_admin(row.repository.id))
        .map(|row| row.repository.id)
        .collect();
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
            created_at_ms: repo.created_at_ms,
            metadata_updated_at_ms: updates.get(&repo.id).copied(),
            owner: &repo.config.owner,
            name: &repo.config.name,
            description: &repo.config.description,
            topics: topics.get(&repo.id).map_or(&[], Vec::as_slice),
            archived: lifecycle.archived,
            visibility: visibility.get(&repo.id).copied().unwrap_or_default(),
            fork: repo.forked_from.is_some(),
            can_admin: admins.contains(&repo.id),
            team_access: team_grants.get(&repo.id).map(|&(access, _, _)| access),
            team_access_inherited: team_grants
                .get(&repo.id)
                .is_some_and(|&(_, _, inherited)| inherited),
            can_manage_team_access: team_grants
                .get(&repo.id)
                .is_some_and(|&(_, owner, _)| owner)
                && repo.forked_from.is_none(),
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
