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
            SearchInput,
            collaboration::SubjectKind,
            search::index::{
                Sort,
                work::{self, ListWorkspaceWork},
            },
        },
    },
    config::valid_repository_component,
    repository::{Repository, access},
    server::Server,
};

use super::{catalog, discussion_search, read_admission};

const CONCURRENT_REPOSITORIES: usize = 8;

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Relation {
    #[default]
    Authored,
    Assigned,
    ReviewRequested,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StateFilter {
    #[default]
    Open,
    Closed,
    All,
}

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchParameters {
    q: Option<String>,
    scope: Option<String>,
    kind: Option<SubjectKind>,
    sort: Option<Sort>,
    limit: Option<u8>,
    after: Option<String>,
}

#[derive(Serialize)]
enum Selection<'a> {
    All,
    Owner(&'a str),
    Repository(&'a str, &'a str),
}

impl<'a> Selection<'a> {
    fn parse(raw: Option<&'a str>) -> Result<Self> {
        match raw.map(str::trim).filter(|scope| !scope.is_empty()) {
            None => Ok(Self::All),
            Some(value) => match value.split_once('/') {
                Some((owner, name))
                    if valid_repository_component(owner) && valid_repository_component(name) =>
                {
                    Ok(Self::Repository(owner, name))
                }
                None if valid_repository_component(value) => Ok(Self::Owner(value)),
                _ => Err(Error::Invalid(
                    "Use an owner or owner/repository to limit search",
                )),
            },
        }
    }

    fn includes(&self, repo: &Repository) -> bool {
        match self {
            Self::All => true,
            Self::Owner(owner) => repo.config.owner.eq_ignore_ascii_case(owner),
            Self::Repository(owner, name) => {
                repo.config.owner.eq_ignore_ascii_case(owner)
                    && repo.config.name.eq_ignore_ascii_case(name)
            }
        }
    }
}

struct Request<'a> {
    selection: Selection<'a>,
    kind: Option<SubjectKind>,
    search: SearchInput,
    personal_review: bool,
    after: Option<&'a str>,
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
    time: u64,
    number: u64,
    kind: SubjectKind,
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
            return Err(Error::Invalid("Invalid work cursor"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Error::Invalid("Invalid work cursor"))?;
        let (tag, payload) = bytes
            .split_at_checked(32)
            .ok_or(Error::Invalid("Invalid work cursor"))?;
        if blake3::keyed_hash(key, payload).as_bytes() != tag {
            return Err(Error::PageChanged);
        }
        serde_json::from_slice(payload).map_err(|_| Error::Invalid("Invalid work cursor"))
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
    request: &Request<'_>,
) -> Result<Scope> {
    let mut repositories: Vec<_> = catalog::visible_matching(server, principal, None, |repo| {
        request.selection.includes(repo)
    })
    .await?
    .into_iter()
    .map(|row| row.repository)
    .collect();
    repositories.sort_unstable_by_key(|repo| repo.id);
    let teams = if request.personal_review {
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
    digest.update(b"workspace-work/v1");
    digest.update(&serde_json::to_vec(&(
        account,
        request.personal_review,
        &request.selection,
        request.kind,
        &request.search,
        &teams,
    ))?);
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
    let kind = params.kind.unwrap_or(SubjectKind::Pull);
    let personal_review = params.relation == Relation::ReviewRequested;
    if personal_review && (kind != SubjectKind::Pull || params.state != StateFilter::Open) {
        return Err(Error::Invalid("Review requests require open pull requests").into_response());
    }
    let relation = match params.relation {
        Relation::Authored => "author:@me",
        Relation::Assigned => "assignee:@me",
        Relation::ReviewRequested => "review-requested:@me",
    };
    let query = match params.state {
        StateFilter::Open => format!("is:open {relation}"),
        StateFilter::Closed => format!("is:closed {relation}"),
        StateFilter::All => relation.into(),
    };
    respond(
        &server,
        &principal,
        Request {
            selection: Selection::All,
            kind: Some(kind),
            search: SearchInput {
                query: Some(query),
                limit: params.limit.unwrap_or(30),
                ..Default::default()
            },
            personal_review,
            after: params.after.as_deref(),
        },
    )
    .await
}

pub(super) async fn search(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<SearchParameters>,
) -> std::result::Result<Response, Response> {
    let selection =
        Selection::parse(params.scope.as_deref()).map_err(IntoResponse::into_response)?;
    let query = app::search_query(params.q.as_deref()).map_err(IntoResponse::into_response)?;
    if query.is_none() && matches!(selection, Selection::All) {
        return Err(
            Error::Invalid("Enter search text, a qualifier, or a repository scope").into_response(),
        );
    }
    respond(
        &server,
        &principal,
        Request {
            selection,
            kind: params.kind,
            search: SearchInput {
                query,
                sort: params.sort.unwrap_or_default(),
                limit: params.limit.unwrap_or(30),
                ..Default::default()
            },
            personal_review: false,
            after: params.after.as_deref(),
        },
    )
    .await
}

async fn respond(
    server: &Server,
    principal: &Principal,
    mut request: Request<'_>,
) -> std::result::Result<Response, Response> {
    let account = app::account(principal).map_err(IntoResponse::into_response)?;
    if !(1..=50).contains(&request.search.limit)
        || !matches!(request.search.sort, Sort::Created | Sort::Updated)
    {
        return Err(Error::Invalid("Invalid work sort or page size").into_response());
    }
    let admission = read_admission::acquire_bulk(server).map_err(|_| read_admission::busy())?;
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        discussion_search::bind_people(server, principal, &mut request.search).await?;
        page(server, principal, &account, &request).await
    }).await.map_err(|_| {
        (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({
            "error":{"code":"timeout","message":"Loading work took too long. Try a narrower repository scope."}
        }))).into_response()
    })?.map_err(IntoResponse::into_response)?;
    Ok(read_admission::retain(
        Json(result).into_response(),
        admission,
    ))
}

async fn read(
    server: &Server,
    principal: &Principal,
    request: &Request<'_>,
    scope: &Scope,
    repo: &Repository,
    position: Option<Position>,
) -> Result<Vec<work::Item>> {
    let cell = app::repository_cell(server, repo)?;
    let mut search = request.search.clone();
    let teams = if request.personal_review {
        search.review_teams.mine = repo
            .organization
            .and_then(|id| scope.teams.get(&id))
            .cloned()
            .unwrap_or_default();
        None
    } else {
        discussion_search::bind_repository(server, principal, repo, &mut search).await?
    };
    let types = discussion_search::issue_types::bind(server, repo, &mut search).await?;
    let items = app::query_output(
        cell.client
            .query::<ListWorkspaceWork>(
                &cell.target,
                None,
                work::Input {
                    kind: request.kind,
                    search,
                    after: position.map(|key| work::After {
                        time: key.time,
                        number: key.number,
                        kind: key.kind,
                        // A repository UUID breaks same-time, same-subject ties.
                        inclusive: repo.id < key.repository,
                    }),
                },
            )
            .await,
    )?;
    discussion_search::issue_types::verify(server, repo, types.as_ref()).await?;
    if let Some(teams) = teams {
        teams.verify(server, principal, repo).await?;
    }
    Ok(items)
}

async fn page(
    server: &Server,
    principal: &Principal,
    account: &IdentityKey,
    request: &Request<'_>,
) -> Result<Page> {
    let initial = scope(server, principal, account, request).await?;
    let position = request
        .after
        .map(|value| Position::decode(value, &initial.key))
        .transpose()?;
    let snapshot = &initial;
    let mut pages = stream::iter(initial.repositories.iter().cloned())
        .map(|repo: Arc<Repository>| async move {
            let items = read(server, principal, request, snapshot, &repo, position).await?;
            Ok::<_, Error>((repo, items))
        })
        .buffer_unordered(CONCURRENT_REPOSITORIES);
    let mut items = Vec::new();
    let limit = usize::from(request.search.limit);
    let sort = request.search.sort;
    while let Some((repo, rows)) = pages.try_next().await? {
        items.extend(rows.into_iter().map(|item| Row {
            repository: RepositoryLink {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
            },
            item,
        }));
        items.sort_unstable_by_key(|row| Reverse((row.item.key(sort), row.repository.id)));
        items.truncate(limit + 1);
    }
    let current = scope(server, principal, account, request).await?;
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
                    time: row.item.key(sort).0,
                    number: row.item.number,
                    kind: row.item.kind,
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
