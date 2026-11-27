#![expect(
    clippy::result_large_err,
    reason = "handlers and their helpers return rejections as ready axum responses"
)]

use std::{collections::BTreeMap, sync::Arc, time::Duration};

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
        directory::{ActorSummary, IdentityKey},
        repository::{
            SearchInput,
            collaboration::{SubjectKind, SubjectState},
            search::index::{
                Direction, Sort,
                work::{self, ListWorkspaceWork},
            },
        },
    },
    config::valid_repository_component,
    repository::{Repository, access},
    server::Server,
};

use super::{catalog, discussion_search, profiles::attribution::Authors, read_admission};

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
    sort: Option<Sort>,
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
    direction: Option<Direction>,
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
struct AuthorView {
    name: String,
    profile: Option<ActorSummary>,
}

struct PendingRow {
    repository: RepositoryLink,
    item: work::Item,
}

#[derive(Serialize)]
struct Row {
    repository: RepositoryLink,
    kind: SubjectKind,
    number: u64,
    title: String,
    state: SubjectState,
    draft: bool,
    created_at: u64,
    updated_at: u64,
    sort_value: u64,
    comments: u64,
    author: AuthorView,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Position {
    value: u64,
    number: u64,
    kind: SubjectKind,
    repository: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filter_snapshot: Option<[u8; 32]>,
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
    repositories: Vec<(Arc<Repository>, Principal)>,
    teams: BTreeMap<Uuid, Vec<Uuid>>,
    key: [u8; 32],
}

async fn scope(
    server: &Server,
    principal: &Principal,
    account: &IdentityKey,
    request: &Request<'_>,
) -> Result<Scope> {
    let query = crate::cells::repository::search::Query::parse(request.search.query.as_deref())
        .map_err(Error::Invalid)?;
    let needs_member = query.needs_pull_heads() || query.uses_merge_queue();
    let mut repositories: Vec<_> = catalog::visible_matching(server, principal, None, |repo| {
        request.selection.includes(repo)
    })
    .await?
    .into_iter()
    .filter(|row| !needs_member || !row.principal.is_public_reader(row.repository.id))
    .map(|row| (row.repository, row.principal))
    .collect();
    repositories.sort_unstable_by_key(|(repo, _)| repo.id);
    let teams = if request.personal_review {
        access::organization_grants(
            server,
            principal,
            repositories
                .iter()
                .filter_map(|(repo, _)| repo.organization.map(|id| (id, true)))
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
    digest.update(b"workspace-work/v2");
    digest.update(&serde_json::to_vec(&(
        account,
        request.personal_review,
        &request.selection,
        request.kind,
        &request.search,
        &teams,
    ))?);
    for (repo, reader) in &repositories {
        digest.update(repo.id.as_bytes());
        digest.update(&[u8::from(reader.is_public_reader(repo.id))]);
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
                sort: params.sort.unwrap_or_default(),
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
                direction: params.direction.unwrap_or_default(),
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
    if !(1..=50).contains(&request.search.limit) || !request.search.sort.is_workspace_sort() {
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
) -> Result<work::Page> {
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
                        value: key.value,
                        number: key.number,
                        kind: key.kind,
                        // The repository UUID breaks ties for the same subject key.
                        inclusive: request
                            .search
                            .direction
                            .compare(repo.id, key.repository)
                            .is_gt(),
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
        .map(|(repo, reader): (Arc<Repository>, Principal)| async move {
            let items = read(server, &reader, request, snapshot, &repo, position).await?;
            Ok::<_, Error>((repo, items))
        })
        .buffer_unordered(CONCURRENT_REPOSITORIES);
    let mut items = Vec::new();
    let limit = usize::from(request.search.limit);
    let direction = request.search.direction;
    let mut filter_snapshots = BTreeMap::new();
    while let Some((repo, page)) = pages.try_next().await? {
        if let Some(snapshot) = page.filter_snapshot {
            filter_snapshots.insert(repo.id, snapshot);
        }
        items.extend(page.items.into_iter().map(|item| PendingRow {
            repository: RepositoryLink {
                id: repo.id,
                owner: repo.config.owner.clone(),
                name: repo.config.name.clone(),
            },
            item,
        }));
        items.sort_unstable_by(|left, right| {
            direction.compare(
                (left.item.key(), left.repository.id),
                (right.item.key(), right.repository.id),
            )
        });
        items.truncate(limit + 1);
    }
    let filter_snapshot = if filter_snapshots.is_empty() {
        None
    } else {
        Some(*blake3::hash(&serde_json::to_vec(&filter_snapshots)?).as_bytes())
    };
    if position.is_some_and(|position| position.filter_snapshot != filter_snapshot) {
        return Err(Error::PageChanged);
    }
    let next = if items.len() > limit {
        items.truncate(limit);
        items
            .last()
            .map(|row| {
                Position {
                    value: row.item.sort_value,
                    number: row.item.number,
                    kind: row.item.kind,
                    repository: row.repository.id,
                    filter_snapshot,
                }
                .encode(&initial.key)
            })
            .transpose()?
    } else {
        None
    };
    let authors =
        Authors::load(server, principal, items.iter().map(|row| &row.item.author)).await?;
    let current = scope(server, principal, account, request).await?;
    app::account(principal)?;
    if current.key != initial.key {
        return Err(Error::PageChanged);
    }
    let items = items
        .into_iter()
        .map(|row| {
            let view = authors.view(&row.item.author);
            Row {
                repository: row.repository,
                kind: row.item.kind,
                number: row.item.number,
                title: row.item.title,
                state: row.item.state,
                draft: row.item.draft,
                created_at: row.item.created_at,
                updated_at: row.item.updated_at,
                sort_value: row.item.sort_value,
                comments: row.item.comments,
                author: AuthorView {
                    name: view.name.to_owned(),
                    profile: view.profile.cloned(),
                },
            }
        })
        .collect();
    Ok(Page { items, next })
}

#[cfg(test)]
mod tests;
