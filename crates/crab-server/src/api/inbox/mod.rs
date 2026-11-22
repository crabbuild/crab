use std::{cmp::Reverse, collections::HashSet, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    middleware,
    routing::{get, patch},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::{catalog, profiles::attribution::Authors},
    app::{self, Error, Result},
    auth::Principal,
    cells::{directory::IdentityKey, repository::collaboration::notifications as stored},
    repository::Repository,
    server::Server,
};

const MAX_UNSCOPED_REPOS: usize = 32;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/inbox", get(page))
        .route("/api/inbox/{repository}/{event}", patch(mark_read))
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    repository: Option<Uuid>,
    unread: Option<bool>,
    cursor: Option<String>,
    limit: Option<u8>,
}

#[derive(Clone, Copy, Deserialize, Serialize, Eq, Ord, PartialOrd, PartialEq)]
#[serde(deny_unknown_fields)]
struct Cursor {
    at: u64,
    repository: Uuid,
    event: u64,
}

impl Cursor {
    fn for_repository(self, repo: Uuid) -> stored::Position {
        let event = match repo.cmp(&self.repository) {
            // On a timestamp tie the repository UUID sorts before event ID.
            std::cmp::Ordering::Less => u64::MAX,
            std::cmp::Ordering::Equal => self.event,
            std::cmp::Ordering::Greater => 0,
        };
        stored::Position { at: self.at, event }
    }
}

fn address(repo: &Repository) -> Value {
    json!({"id":repo.id,"owner":repo.config.owner,"name":repo.config.name})
}

async fn page(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(filters): Query<Filters>,
) -> Result<Json<Value>> {
    let account = app::account(&principal)?;
    let unread = filters.unread.unwrap_or(true);
    let limit = filters.limit.unwrap_or(25);
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Inbox pages require 1–50 notifications"));
    }
    let tokens = Tokens::new(&server.cursor_key, &account, unread, filters.repository)?;
    let before = filters
        .cursor
        .as_deref()
        .map(|value| tokens.decode(value))
        .transpose()?;
    let visible = catalog::visible(&server, &principal, None, None).await?;
    let selected: Vec<_> = visible
        .iter()
        .map(|visible| visible.repository.clone())
        .filter(|repo| filters.repository.is_none_or(|id| id == repo.id))
        .collect();
    if filters.repository.is_some() && selected.is_empty() {
        return Err(Error::NotFound);
    }
    if selected.len() > MAX_UNSCOPED_REPOS {
        return Ok(Json(json!({
            "items": [], "next": null, "scope_required": true,
            "repositories": visible.iter().map(|visible| address(&visible.repository)).collect::<Vec<_>>(),
        })));
    }
    let server_ref = server.as_ref();
    let account_ref = &account;
    let shards = futures_util::stream::iter(selected.into_iter().map(|repo| async move {
        let cell = app::repository_cell(server_ref, &repo)?;
        let items = app::query_output(
            cell.client
                .query::<stored::List>(
                    cell.target,
                    None,
                    stored::Query {
                        account: account_ref.clone(),
                        unread,
                        before: before.map(|cursor| cursor.for_repository(repo.id)),
                        limit: limit + 1,
                    },
                )
                .await,
        )?;
        Ok::<_, Error>((repo, items))
    }))
    .buffer_unordered(8)
    .try_collect::<Vec<_>>()
    .await?;
    let mut rows = Vec::new();
    for (repo, items) in shards {
        rows.extend(items.into_iter().map(|entry| {
            let sort = Cursor {
                at: entry.event.created_at,
                repository: repo.id,
                event: entry.event.number,
            };
            (sort, repo.clone(), entry)
        }));
    }
    // Recheck access after querying the repository Cells before building the page.
    let visible = catalog::visible(&server, &principal, None, None).await?;
    let allowed: HashSet<_> = visible
        .iter()
        .map(|visible| visible.repository.id)
        .collect();
    rows.retain(|(_, repo, _)| allowed.contains(&repo.id));
    if filters.repository.is_some_and(|id| !allowed.contains(&id)) {
        return Err(Error::NotFound);
    }
    rows.sort_unstable_by_key(|(cursor, _, _)| Reverse(*cursor));
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let next = rows
        .last()
        .filter(|_| more)
        .map(|(cursor, _, _)| tokens.encode(*cursor))
        .transpose()?;
    let authors = Authors::load(
        &server,
        &principal,
        rows.iter().map(|(_, _, entry)| &entry.event.actor),
    )
    .await?;
    let items = rows
        .into_iter()
        .map(|(_, repo, notice)| {
            json!({
                "repository": address(&repo), "event": notice.event.number,
                "at": notice.event.created_at, "target": notice.event.target,
                "title": notice.event.title, "actor": authors.view(&notice.event.actor),
                "reason": notice.reason, "read": notice.read,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "items":items, "next":next, "scope_required":false,
        "repositories":visible.iter().map(|visible| address(&visible.repository)).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    at: u64,
    read: bool,
}

async fn mark_read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path((id, event)): Path<(Uuid, u64)>,
    Json(input): Json<Mark>,
) -> Result<Json<Value>> {
    let account = app::account(&principal)?;
    let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
    app::membership(&server, &principal, &repo).await?;
    let cell = app::repository_cell(&server, &repo)?;
    let changed = app::command_output(
        cell.client
            .command::<stored::SetRead>(
                cell.target,
                app::mutation_identity()?,
                stored::ReadInput {
                    account,
                    position: stored::Position {
                        at: input.at,
                        event: app::number(event)?,
                    },
                    read: input.read,
                },
            )
            .await,
    )?;
    if !changed {
        return Err(Error::NotFound);
    }
    Ok(Json(json!({"read":input.read})))
}

struct Tokens {
    key: [u8; 32],
}

impl Tokens {
    fn new(
        key: &[u8; 32],
        account: &IdentityKey,
        unread: bool,
        repository: Option<Uuid>,
    ) -> Result<Self> {
        let mut hash = blake3::Hasher::new_keyed(key);
        hash.update(b"crab-inbox-cursor-v1\0");
        hash.update(&serde_json::to_vec(&(account, unread, repository))?);
        Ok(Self {
            key: *hash.finalize().as_bytes(),
        })
    }

    fn encode(&self, cursor: Cursor) -> Result<String> {
        let mut bytes = serde_json::to_vec(&cursor)?;
        let digest = blake3::keyed_hash(&self.key, &bytes);
        bytes.extend(digest.as_bytes());
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }

    fn decode(&self, value: &str) -> Result<Cursor> {
        let invalid = Error::Invalid("Invalid cursor for this inbox");
        if value.len() > 256 {
            return Err(invalid);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Error::Invalid("Invalid inbox cursor"))?;
        if bytes.len() <= 32 {
            return Err(invalid);
        }
        let (payload, signature) = bytes.split_at(bytes.len() - 32);
        if blake3::keyed_hash(&self.key, payload) != *signature {
            return Err(invalid);
        }
        serde_json::from_slice(payload).map_err(|_| invalid)
    }
}

#[cfg(test)]
mod tests;
