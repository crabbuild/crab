use std::{cmp::Reverse, collections::BTreeMap, sync::Arc, time::Duration};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        directory::IdentityKey,
        repository::{
            collaboration::SubjectKind,
            search::{
                index::work::{self, Filter, ListPersonalWork, Relation, StateFilter},
                people,
            },
        },
    },
    repository::{Repository, access},
    server::Server,
};

use super::{catalog, read_admission};

const CONCURRENT_REPOSITORIES: usize = 8;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Parameters {
    #[serde(default)]
    relation: Relation,
    kind: Option<SubjectKind>,
    #[serde(default)]
    state: StateFilter,
    limit: Option<u8>,
    after: Option<String>,
}

#[derive(Serialize)]
struct Page {
    items: Vec<Row>,
    next: Option<String>,
}

#[derive(Serialize)]
struct RepositoryLink {
    id: Uuid,
    owner: String,
    name: String,
}

#[derive(Serialize)]
struct Row {
    repository: RepositoryLink,
    #[serde(flatten)]
    item: work::Item,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Position {
    created_at: u64,
    number: u64,
    repository: Uuid,
}

impl Position {
    fn encode(self, key: &[u8; 32]) -> Result<String> {
        let payload = serde_json::to_vec(&self)?;
        let tag = blake3::keyed_hash(key, &payload);
        let mut token = tag.as_bytes().to_vec();
        token.extend(payload);
        Ok(URL_SAFE_NO_PAD.encode(token))
    }

    fn decode(value: &str, key: &[u8; 32]) -> Result<Self> {
        if value.len() > 512 {
            return Err(Error::Invalid("Invalid personal work cursor"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Error::Invalid("Invalid personal work cursor"))?;
        let (tag, payload) = bytes
            .split_at_checked(32)
            .ok_or(Error::Invalid("Invalid personal work cursor"))?;
        if blake3::keyed_hash(key, payload).as_bytes() != tag {
            return Err(Error::PageChanged);
        }
        serde_json::from_slice(payload).map_err(|_| Error::Invalid("Invalid personal work cursor"))
    }
}

struct Scope {
    repositories: Vec<Arc<Repository>>,
    teams: BTreeMap<Uuid, Vec<Uuid>>,
    key: [u8; 32],
}

async fn scope(
    server: &Server,
    principal: &Principal,
    account: &IdentityKey,
    filter: Filter,
) -> Result<Scope> {
    let mut repositories: Vec<_> = catalog::visible(server, principal, None, None)
        .await?
        .into_iter()
        .map(|row| row.repository)
        .collect();
    repositories.sort_unstable_by_key(|repo| repo.id);
    let teams = if filter.relation == Relation::ReviewRequested {
        access::organization_grants(
            server,
            principal,
            repositories
                .iter()
                .filter_map(|repo| repo.organization.map(|id| (id, true)))
                .collect(),
        )
        .await?
        .into_iter()
        .map(|(id, grant)| (id, grant.teams))
        .collect()
    } else {
        BTreeMap::new()
    };
    let mut digest = blake3::Hasher::new_keyed(&server.cursor_key);
    digest.update(b"personal-work/v1");
    digest.update(&serde_json::to_vec(&(account, filter, &teams))?);
    for repo in &repositories {
        digest.update(repo.id.as_bytes());
        digest.update(&repo.ownership_epoch.to_le_bytes());
        digest.update(repo.config.owner.as_bytes());
        digest.update(b"\0");
        digest.update(repo.config.name.as_bytes());
        digest.update(b"\0");
    }
    Ok(Scope {
        repositories,
        teams,
        key: *digest.finalize().as_bytes(),
    })
}

pub(super) async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> std::result::Result<Response, Response> {
    let account = app::account(&principal).map_err(IntoResponse::into_response)?;
    let filter = Filter {
        relation: params.relation,
        kind: params.kind.unwrap_or(SubjectKind::Pull),
        state: params.state,
    };
    let limit = params.limit.unwrap_or(30);
    if !filter.valid() || !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Invalid personal work filter or page size").into_response());
    }
    let admission = read_admission::acquire_bulk(&server).map_err(|_| read_admission::busy())?;
    let result = tokio::time::timeout(
        Duration::from_secs(25),
        page(
            &server,
            &principal,
            &account,
            filter,
            limit,
            params.after.as_deref(),
        ),
    )
    .await
    .map_err(|_| {
        (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({
            "error":{"code":"timeout","message":"Loading your work took too long. Retry shortly."}
        }))).into_response()
    })?
    .map_err(IntoResponse::into_response)?;
    Ok(read_admission::retain(
        Json(result).into_response(),
        admission,
    ))
}

async fn page(
    server: &Server,
    principal: &Principal,
    account: &IdentityKey,
    filter: Filter,
    limit: u8,
    after: Option<&str>,
) -> Result<Page> {
    let initial = scope(server, principal, account, filter).await?;
    let position = after
        .map(|value| Position::decode(value, &initial.key))
        .transpose()?;
    let assignment = if filter.relation == Relation::Assigned {
        access::actor_id(server, account.clone()).await?
    } else {
        None
    };
    let identity = people::identity(&account.issuer, &account.subject).map_err(Error::Cell)?;
    let team_bindings = &initial.teams;
    let mut pages = stream::iter(initial.repositories.iter().cloned())
        .map(|repo: Arc<Repository>| async move {
            let cell = app::repository_cell(server, &repo)?;
            let teams = repo
                .organization
                .and_then(|id| team_bindings.get(&id))
                .cloned()
                .unwrap_or_default();
            let items = app::query_output(
                cell.client
                    .query::<ListPersonalWork>(
                        &cell.target,
                        None,
                        work::Input {
                            filter,
                            identity,
                            account: assignment,
                            teams,
                            after: position.map(|key| work::After {
                                created_at: key.created_at,
                                number: key.number,
                                // A repository UUID breaks same-time, same-number ties.
                                inclusive: repo.id < key.repository,
                            }),
                            limit: limit + 1,
                        },
                    )
                    .await,
            )?;
            Ok::<_, Error>((repo, items))
        })
        .buffer_unordered(CONCURRENT_REPOSITORIES);
    let mut items = Vec::new();
    let limit = usize::from(limit);
    while let Some((repo, rows)) = pages.try_next().await? {
        items.extend(rows.into_iter().map(|item| Row {
            repository: RepositoryLink {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
            },
            item,
        }));
        items.sort_unstable_by_key(|row| {
            Reverse((row.item.created_at, row.item.number, row.repository.id))
        });
        items.truncate(limit + 1);
    }
    let current = scope(server, principal, account, filter).await?;
    app::account(principal)?;
    if current.key != initial.key {
        return Err(Error::PageChanged);
    }
    let next = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .map(|row| {
                Position {
                    created_at: row.item.created_at,
                    number: row.item.number,
                    repository: row.repository.id,
                }
                .encode(&initial.key)
            })
            .transpose()?
    } else {
        None
    };
    Ok(Page { items, next })
}

#[cfg(test)]
mod tests;
