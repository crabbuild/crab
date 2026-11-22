use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State, rejection::JsonRejection},
    middleware,
    routing::{get, put},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::notifications::{
            Entry, List, ListInput, MarkRead, MarkReadInput, Position, Reason,
        },
        repository::{
            collaboration::{SubjectKind, Target},
            search::index::references::{FindReferences, ReferenceInput},
        },
    },
    repository::Repository,
    server::Server,
};

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/unread", get(unread))
        .route("/api/notifications/{id}/read", put(mark_read))
        .layer(DefaultBodyLimit::max(1024))
        .route_layer(middleware::from_fn_with_state(server, app::admit))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    before: Option<String>,
    limit: Option<u8>,
    repository: Option<Uuid>,
    unread: Option<bool>,
    reason: Option<Reason>,
}

#[derive(Serialize)]
struct RepositoryLabel {
    owner: String,
    name: String,
}

#[derive(Serialize)]
struct Item {
    id: Uuid,
    repository: RepositoryLabel,
    title: String,
    href: String,
    sender: String,
    reason: Reason,
    read: bool,
    created_at_ms: u64,
}

#[derive(Serialize)]
struct Page {
    items: Vec<Item>,
    next: Option<String>,
}

fn cursor(value: &str) -> Result<Position> {
    const ERROR: Error = Error::Invalid("Notification cursor is invalid");
    let (timestamp, id) = value.split_once(':').ok_or(ERROR)?;
    if timestamp.len() > 16 || id.len() != 36 {
        return Err(ERROR);
    }
    let position = Position {
        created_at_ms: timestamp.parse().map_err(|_| ERROR)?,
        id: id.parse().map_err(|_| ERROR)?,
    };
    valid_position(position)?;
    Ok(position)
}

fn valid_position(position: Position) -> Result<()> {
    if position.id.is_nil() || position.created_at_ms > app::MAX_NUMBER {
        return Err(Error::Invalid("Notification position is invalid"));
    }
    Ok(())
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(params): Query<Parameters>,
) -> Result<Json<Page>> {
    let limit = usize::from(params.limit.unwrap_or(20));
    if !(1..=50).contains(&limit) {
        return Err(Error::Invalid("Notification pages require 1–50 items"));
    }
    let before = params.before.as_deref().map(cursor).transpose()?;
    if let Some(id) = params.repository {
        let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
        if !may_read(&server, &principal, &repo).await? {
            return Err(Error::NotFound);
        }
    }
    let entries = entries(&server, &principal, before).await?;
    // Read one account's bounded history and authorize by repository, rather
    // than scanning private repositories or checking the same grant per item.
    let mut groups = HashMap::<Uuid, Vec<(usize, Entry)>>::new();
    for (order, entry) in entries.into_iter().enumerate() {
        if params
            .repository
            .is_some_and(|id| id != entry.notice.repository)
            || (params.unread == Some(true) && entry.read)
            || params.reason.is_some_and(|reason| reason != entry.reason)
        {
            continue;
        }
        groups
            .entry(entry.notice.repository)
            .or_default()
            .push((order, entry));
    }
    let batches: Vec<Vec<(usize, Item)>> = stream::iter(groups)
        .map(|(id, entries)| repository_items(&server, &principal, id, entries))
        .buffer_unordered(8)
        .try_collect()
        .await?;
    app::account(&principal)?;
    let mut items: Vec<_> = batches.into_iter().flatten().collect();
    items.sort_unstable_by_key(|(order, _)| *order);
    // A cursor always points to a returned item. Hidden entries do not reveal
    // timestamps, IDs, counts, or empty pages with private continuations.
    let next = (items.len() > limit).then(|| {
        let last = &items[limit - 1].1;
        format!("{}:{}", last.created_at_ms, last.id)
    });
    let items = items
        .into_iter()
        .take(limit)
        .map(|(_, item)| item)
        .collect();
    Ok(Json(Page { items, next }))
}

async fn entries(
    server: &Server,
    principal: &Principal,
    before: Option<Position>,
) -> Result<Vec<Entry>> {
    let identity = app::account(principal)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::query_output(
        cells
            .query::<List>(Address::Directory, None, ListInput { identity, before })
            .await,
    )
}

#[derive(Serialize)]
struct Unread {
    unread: bool,
}

async fn unread(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Unread>> {
    let repositories: HashSet<_> = entries(&server, &principal, None)
        .await?
        .into_iter()
        .filter(|entry| !entry.read)
        .map(|entry| entry.notice.repository)
        .collect();
    let mut accessible = stream::iter(repositories)
        .filter_map(|id| std::future::ready(server.repositories.by_id(id)))
        .map(|repo| {
            let (server, principal) = (&server, &principal);
            async move { may_read(server, principal, &repo).await }
        })
        .buffer_unordered(8);
    while let Some(current) = accessible.try_next().await? {
        if current {
            app::account(&principal)?;
            return Ok(Json(Unread { unread: true }));
        }
    }
    app::account(&principal)?;
    Ok(Json(Unread { unread: false }))
}

async fn may_read(server: &Server, principal: &Principal, repo: &Repository) -> Result<bool> {
    match app::membership(server, principal, repo).await {
        Ok(_) => Ok(true),
        Err(Error::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn repository_items(
    server: &Server,
    principal: &Principal,
    id: Uuid,
    entries: Vec<(usize, Entry)>,
) -> Result<Vec<(usize, Item)>> {
    let Some(repo) = server.repositories.by_id(id) else {
        return Ok(Vec::new());
    };
    if !may_read(server, principal, &repo).await? {
        return Ok(Vec::new());
    }
    let cell = app::repository_cell(server, &repo)?;
    let owner = &repo.config.owner;
    let name = &repo.config.name;
    let mut items = Vec::new();
    let mut cache = HashMap::new();
    for (order, entry) in entries {
        let notice = entry.notice;
        let (kind, number, files, contribution) = location(notice.target);
        let sources = match cache.entry(number) {
            std::collections::hash_map::Entry::Occupied(found) => found.into_mut(),
            std::collections::hash_map::Entry::Vacant(missing) => {
                missing.insert(app::query_output(
                    cell.client
                        .query::<FindReferences>(
                            &cell.target,
                            None,
                            ReferenceInput {
                                search: number.to_string(),
                                exact: true,
                            },
                        )
                        .await,
                )?)
            }
        };
        let Some(source) = sources.iter().find(|item| item.subject.kind == kind) else {
            continue;
        };
        let path = match kind {
            SubjectKind::Issue => "issues",
            SubjectKind::Pull => "pull",
        };
        items.push((
            order,
            Item {
                id: notice.id,
                repository: RepositoryLabel {
                    owner: owner.clone(),
                    name: name.clone(),
                },
                title: source.title.clone(),
                href: format!(
                    "/{owner}/{name}/{path}/{number}{files}?repository_id={id}{contribution}"
                ),
                sender: notice.sender,
                reason: entry.reason,
                read: entry.read,
                created_at_ms: notice.created_at_ms,
            },
        ));
    }
    // Title reads await a different Cell. Recheck the current grant once
    // per repository before presenting its whole batch.
    if !may_read(server, principal, &repo).await? {
        return Ok(Vec::new());
    }
    Ok(items)
}

fn location(target: Target) -> (SubjectKind, u64, &'static str, String) {
    match target {
        Target::Issue { number } => (SubjectKind::Issue, number, "", String::new()),
        Target::Pull { number } => (SubjectKind::Pull, number, "", String::new()),
        Target::IssueComment { issue, number } => (
            SubjectKind::Issue,
            issue,
            "",
            format!("#issuecomment-{number}"),
        ),
        Target::PullComment { pull, number } => (
            SubjectKind::Pull,
            pull,
            "",
            format!("#issuecomment-{number}"),
        ),
        Target::PullReview { pull, number } => (
            SubjectKind::Pull,
            pull,
            "",
            format!("#pullrequestreview-{number}"),
        ),
        Target::PullThread { pull, number } => (
            SubjectKind::Pull,
            pull,
            "/files",
            format!("&thread={number}#review-thread-{number}"),
        ),
        Target::PullReply {
            pull,
            thread,
            number,
        } => (
            SubjectKind::Pull,
            pull,
            "/files",
            format!("&thread={thread}&reply={number}#review-thread-{thread}"),
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    created_at_ms: u64,
    read: Option<bool>,
}

async fn mark_read(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
    input: std::result::Result<Json<ReadRequest>, JsonRejection>,
) -> Result<Json<()>> {
    let identity = app::account(&principal)?;
    let Json(input) = input.map_err(Error::Body)?;
    let position = Position {
        id,
        created_at_ms: input.created_at_ms,
    };
    valid_position(position)?;
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::command_output(
        cells
            .command::<MarkRead>(
                Address::Directory,
                app::mutation_identity()?,
                MarkReadInput {
                    identity,
                    position,
                    read: input.read.unwrap_or(true),
                },
            )
            .await,
    )?;
    app::account(&principal)?;
    // Unknown and already-read notices have the same response as a change.
    Ok(Json(()))
}

#[cfg(test)]
mod delivery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cells::directory::notifications::{
            Access, Deliver, Delivery, HandlesAt, Notice, Recipient, ResolveHandlesAt,
        },
        test_support::http::Harness,
    };
    use axum::http::StatusCode;
    use reqwest::Method;
    use serde_json::{Value, json};

    #[tokio::test]
    async fn notices_are_private_mark_read_is_idempotent_and_revocation_hides_source() {
        let h = Harness::new(false).await;
        let alice = h.login().await;
        h.write(
            Method::POST,
            "/api/repos/team/private/issues",
            &alice,
            json!({"request_id": Uuid::now_v7(), "title": "Issue mentioned", "body": ""}),
        )
        .await
        .error_for_status()
        .unwrap();
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        let profile: Value = h
            .write(
                Method::POST,
                "/api/profile",
                &bob,
                json!({"request_id": Uuid::now_v7(), "handle": "bob", "name": "Bob"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let actor = serde_json::from_value::<Uuid>(profile["id"].clone()).unwrap();
        let updated = profile["updated_at_ms"].as_u64().unwrap();
        let client = h.server.cells().unwrap();
        for (before_ms, expected) in [(updated, None), (updated + 1, Some(actor))] {
            let resolved = client
                .query::<ResolveHandlesAt>(
                    Address::Directory,
                    None,
                    HandlesAt {
                        names: vec!["bob".into(), "missing".into()],
                        before_ms,
                    },
                )
                .await
                .unwrap()
                .output;
            assert_eq!(resolved, vec![expected, None]);
        }
        let repo = h.server.repositories.values()[0].clone();
        let notice = Notice {
            id: Uuid::now_v7(),
            repository: repo.id,
            target: Target::Issue { number: 1 },
            created_at_ms: 100,
            sender: "Alice".into(),
        };
        let mut delivery = Delivery {
            notice: notice.clone(),
            recipients: vec![Recipient {
                actor,
                reason: Reason::Mention,
                access: Access::Direct,
            }],
        };
        let put_read = format!("/api/notifications/{}/read", notice.id);
        for (attempt, access) in [Access::Direct, Access::Group, Access::Direct]
            .into_iter()
            .enumerate()
        {
            delivery.recipients[0].access = access;
            client
                .command::<Deliver>(
                    Address::Directory,
                    app::mutation_identity().unwrap(),
                    delivery.clone(),
                )
                .await
                .unwrap();
            assert_eq!(
                h.json("/api/notifications", &alice).await,
                json!({"items": [], "next": null})
            );
            assert_eq!(
                h.json("/api/notifications/unread", &bob).await,
                json!({"unread": attempt == 0})
            );
            let items = h.json("/api/notifications", &bob).await;
            assert_eq!(
                items,
                json!({"next": null, "items": [{
                    "id": notice.id, "repository": {"owner": "team", "name": "private"},
                    "title": "Issue mentioned",
                    "href": format!("/team/private/issues/1?repository_id={}", repo.id),
                    "sender": "Alice", "reason": "mention", "read": attempt > 0,
                    "created_at_ms": notice.created_at_ms
                }]})
            );
            assert_eq!(
                h.write(Method::PUT, &put_read, &bob, json!({"created_at_ms": 100}))
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        let mut members = repo.config.members.clone();
        members.retain(|member| member.subject != "bob-id");
        h.server
            .catalog()
            .unwrap()
            .set_members("team", "private", members, true)
            .await
            .unwrap();
        assert_eq!(
            h.json("/api/notifications", &bob).await,
            json!({"items": [], "next": null})
        );
        h.close().await;
    }
}
