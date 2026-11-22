use std::{collections::HashMap, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    middleware,
    routing::{get, post, put},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app::{self, Error, Result},
    auth::Principal,
    cells::{
        Address,
        directory::{
            IdentityKey,
            notifications::{
                Entry, List, ListInput, NoticeTarget, Position, Reason, ReleaseKind,
                State as NotificationState, Thread, ThreadPage, View,
            },
        },
        repository::{
            collaboration::{
                Subject, SubjectKind, Target,
                watches::{self, Mode},
            },
            search::index::references::{FindReferences, ReferenceInput},
        },
    },
    repository::Repository,
    server::Server,
};

mod state;
mod subscriptions;
mod unsubscribe;

pub(super) fn routes(server: Arc<Server>) -> Router<Arc<Server>> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/unread", get(unread))
        .route("/api/notifications/{id}/state", put(state::single))
        .route(
            "/api/repos/{owner}/{name}/subscription",
            get(subscriptions::get).put(subscriptions::set),
        )
        .route(
            "/api/repos/{owner}/{name}/{discussion}/{number}/subscription",
            get(subscriptions::get_thread).put(subscriptions::set_thread),
        )
        .layer(DefaultBodyLimit::max(1024))
        .merge(
            Router::new()
                .route("/api/notifications/bulk", post(state::bulk))
                .route("/api/notifications/unsubscribe", post(unsubscribe::apply))
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
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
    view: Option<View>,
}

#[derive(Serialize)]
struct RepositoryLabel {
    id: Uuid,
    owner: String,
    name: String,
}

#[derive(Serialize)]
#[serde(untagged)]
enum InboxSubject {
    Discussion(Subject),
    Release { kind: ReleaseKind, number: u64 },
}

#[derive(Serialize)]
struct Item {
    id: Uuid,
    repository: RepositoryLabel,
    subject: InboxSubject,
    title: String,
    href: String,
    sender: String,
    reason: Reason,
    #[serde(flatten)]
    state: NotificationState,
    updated: Position,
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
    let mut before = params.before.as_deref().map(cursor).transpose()?;
    if let Some(id) = params.repository {
        let repo = server.repositories.by_id(id).ok_or(Error::NotFound)?;
        if !may_read(&server, &principal, &repo).await? {
            return Err(Error::NotFound);
        }
    }
    let actor = app::account(&principal)?;
    let view = params.view.unwrap_or_default();
    let mut items = Vec::new();
    loop {
        let page = entries(&server, actor.clone(), before, view).await?;
        // Authorize each repository batch. Cell cursors can cross private
        // rows, so the HTTP cursor is only made from a visible item.
        let mut groups = HashMap::<Uuid, Vec<(usize, Thread)>>::new();
        for (order, thread) in page.threads.into_iter().enumerate() {
            let entry = &thread.entry;
            if params
                .repository
                .is_some_and(|id| id != entry.notice.repository)
                || (params.unread == Some(true) && thread.state.read)
                || params.reason.is_some_and(|reason| reason != entry.reason)
            {
                continue;
            }
            groups
                .entry(entry.notice.repository)
                .or_default()
                .push((order, thread));
        }
        let batches: Vec<Vec<(usize, Item)>> = stream::iter(groups)
            .map(|(id, entries)| repository_items(&server, &principal, &actor, id, view, entries))
            .buffer_unordered(8)
            .try_collect()
            .await?;
        let mut visible: Vec<_> = batches.into_iter().flatten().collect();
        visible.sort_unstable_by_key(|(order, _)| *order);
        items.extend(visible.into_iter().map(|(_, item)| item));
        if items.len() > limit || page.next.is_none() {
            break;
        }
        before = page.next;
    }
    app::account(&principal)?;
    // A cursor always points to a returned item. Hidden entries do not reveal
    // timestamps, IDs, counts, or empty pages with private continuations.
    let next = (items.len() > limit).then(|| {
        let last = &items[limit - 1];
        format!("{}:{}", last.updated.created_at_ms, last.updated.id)
    });
    items.truncate(limit);
    Ok(Json(Page { items, next }))
}

async fn entries(
    server: &Server,
    identity: IdentityKey,
    before: Option<Position>,
    view: View,
) -> Result<ThreadPage> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::query_output(
        cells
            .query::<List>(
                Address::Directory,
                None,
                ListInput {
                    identity,
                    before,
                    view,
                },
            )
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
    let actor = app::account(&principal)?;
    let mut before = None;
    loop {
        let page = entries(&server, actor.clone(), before, View::Inbox).await?;
        let mut repositories = HashMap::<Uuid, Vec<Entry>>::new();
        for thread in page.threads.into_iter().filter(|thread| !thread.state.read) {
            repositories
                .entry(thread.entry.notice.repository)
                .or_default()
                .push(thread.entry);
        }
        let mut accessible = stream::iter(repositories)
            .map(|(id, items)| {
                let (server, principal, actor) = (&server, &principal, &actor);
                async move {
                    let Some(repo) = server.repositories.by_id(id) else {
                        return Ok(false);
                    };
                    if !may_notify(server, principal, &repo, actor).await? {
                        return Ok(false);
                    }
                    let entries: Vec<_> = items.iter().collect();
                    let sources =
                        resolve_sources(server, &repo, actor, View::Inbox, &entries).await?;
                    let visible = !sources.releases.is_empty()
                        || sources.discussions.into_iter().any(|visible| visible);
                    Ok::<_, Error>(visible && may_notify(server, principal, &repo, actor).await?)
                }
            })
            .buffer_unordered(8);
        while let Some(current) = accessible.try_next().await? {
            if current {
                app::account(&principal)?;
                return Ok(Json(Unread { unread: true }));
            }
        }
        before = page.next;
        if before.is_none() {
            break;
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

async fn may_notify(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    actor: &IdentityKey,
) -> Result<bool> {
    Ok(may_read(server, principal, repo).await?
        && subscriptions::read(server, repo, actor.clone()).await?.mode != Mode::Muted)
}

struct ResolvedSources {
    discussions: Vec<bool>,
    releases: HashMap<u64, watches::ReleaseSource>,
}

async fn resolve_sources(
    server: &Server,
    repo: &Repository,
    actor: &IdentityKey,
    view: View,
    entries: &[&Entry],
) -> Result<ResolvedSources> {
    let cell = app::repository_cell(server, repo)?;
    let mut activity = Vec::new();
    let mut numbers = Vec::new();
    for entry in entries {
        match entry.notice.target {
            NoticeTarget::Discussion(target) => activity.push(watches::InboxActivity {
                subject: target.subject(),
                updated: entry.notice.position(),
                resumed: entry.resumed,
            }),
            NoticeTarget::Release { number, .. } => numbers.push(number),
        }
    }
    let discussions = async {
        if view != View::Inbox {
            return Ok(vec![true; activity.len()]);
        }
        let mut visibility = Vec::with_capacity(activity.len());
        for batch in activity.chunks(50) {
            let shown = app::query_output(
                cell.client
                    .query::<watches::InboxVisibility>(
                        &cell.target,
                        None,
                        watches::InboxVisibilityInput {
                            actor: actor.clone(),
                            items: batch.to_vec(),
                        },
                    )
                    .await,
            )?;
            if shown.len() != batch.len() {
                return Err(Error::CellContract(
                    "Notification visibility count does not match",
                ));
            }
            visibility.extend(shown);
        }
        Ok(visibility)
    };
    let releases = async {
        let mut sources = HashMap::new();
        for batch in numbers.chunks(50) {
            let shown = app::query_output(
                cell.client
                    .query::<watches::PublishedReleases>(&cell.target, None, batch.to_vec())
                    .await,
            )?;
            if shown.len() != batch.len() {
                return Err(Error::CellContract(
                    "Notification release count does not match",
                ));
            }
            sources.extend(
                batch
                    .iter()
                    .copied()
                    .zip(shown)
                    .filter_map(|(number, source)| Some((number, source?))),
            );
        }
        Ok(sources)
    };
    let (discussions, releases) = tokio::try_join!(discussions, releases)?;
    Ok(ResolvedSources {
        discussions,
        releases,
    })
}

async fn repository_items(
    server: &Server,
    principal: &Principal,
    actor: &IdentityKey,
    id: Uuid,
    view: View,
    entries: Vec<(usize, Thread)>,
) -> Result<Vec<(usize, Item)>> {
    let Some(repo) = server.repositories.by_id(id) else {
        return Ok(Vec::new());
    };
    if !may_notify(server, principal, &repo, actor).await? {
        return Ok(Vec::new());
    }
    let sources = resolve_sources(
        server,
        &repo,
        actor,
        view,
        &entries
            .iter()
            .map(|(_, thread)| &thread.entry)
            .collect::<Vec<_>>(),
    )
    .await?;
    let mut discussions = sources.discussions.into_iter();
    let cell = app::repository_cell(server, &repo)?;
    let owner = &repo.config.owner;
    let name = &repo.config.name;
    let mut items = Vec::new();
    let mut cache = HashMap::new();
    for (order, thread) in entries {
        let entry = thread.entry;
        let notice = entry.notice;
        let (subject, title, href) = match notice.target {
            NoticeTarget::Discussion(target) => {
                if discussions.next() != Some(true) {
                    continue;
                }
                let (kind, number, files, contribution) = location(target);
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
                (
                    InboxSubject::Discussion(target.subject()),
                    source.title.clone(),
                    format!(
                        "/{owner}/{name}/{path}/{number}{files}?repository_id={id}{contribution}"
                    ),
                )
            }
            NoticeTarget::Release { kind, number } => {
                let Some(source) = sources.releases.get(&number) else {
                    continue;
                };
                let tag: String =
                    url::form_urlencoded::byte_serialize(source.tag.as_bytes()).collect();
                (
                    InboxSubject::Release { kind, number },
                    source.title.clone(),
                    format!(
                        "/{owner}/{name}/releases/tag/{tag}?repository_id={id}&release_number={number}"
                    ),
                )
            }
        };
        items.push((
            order,
            Item {
                id: notice.thread_id(),
                updated: notice.position(),
                repository: RepositoryLabel {
                    id,
                    owner: owner.clone(),
                    name: name.clone(),
                },
                subject,
                title,
                href,
                sender: notice.sender,
                reason: entry.reason,
                state: thread.state,
            },
        ));
    }
    // Source reads await a different Cell. Recheck the current grant once
    // per repository before presenting its whole batch.
    if !may_notify(server, principal, &repo, actor).await? {
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

#[cfg(test)]
mod delivery_tests;

#[cfg(test)]
mod watch_tests;

#[cfg(test)]
mod triage_tests;

#[cfg(test)]
mod bulk_tests;

#[cfg(test)]
mod unsubscribe_tests;

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
            target: Target::Issue { number: 1 }.into(),
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
        let put_read = format!("/api/notifications/{}/state", notice.thread_id());
        let expected =
            json!({"repository_id":repo.id,"expected":notice.position(),"action":{"read":true}});
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
                    "id": notice.thread_id(), "repository": {"id":repo.id, "owner": "team", "name": "private"},
                    "subject": {"kind":"issue","number":1},
                    "title": "Issue mentioned",
                    "href": format!("/team/private/issues/1?repository_id={}", repo.id),
                    "sender": "Alice", "reason": "mention", "read": attempt > 0, "saved": false, "done": false,
                    "updated": notice.position()
                }]})
            );
            assert_eq!(
                h.write(Method::PUT, &put_read, &bob, expected.clone())
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        delivery.notice = Notice {
            id: Uuid::now_v7(),
            target: Target::IssueComment {
                issue: 1,
                number: 9,
            }
            .into(),
            created_at_ms: 101,
            ..notice
        };
        delivery.recipients[0].reason = Reason::Subscribed;
        client
            .command::<Deliver>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                delivery.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            h.write(Method::PUT, &put_read, &bob, expected)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let row = h
            .json("/api/notifications?reason=mention&unread=true", &bob)
            .await;
        let [item] = row["items"].as_array().unwrap().as_slice() else {
            panic!("a new update must leave one unread conversation");
        };
        assert_eq!(item["id"], json!(delivery.notice.thread_id()));
        assert_eq!(item["updated"], json!(delivery.notice.position()));
        assert!(item["href"].as_str().unwrap().ends_with("#issuecomment-9"));
        let before_update = format!("/api/notifications?before=101:{}", delivery.notice.id);
        assert_eq!(h.json(&before_update, &bob).await["items"], json!([]));
        assert_eq!(
            h.write(
                Method::PUT,
                &put_read,
                &bob,
                json!({"repository_id":repo.id,"expected":item["updated"],"action":{"read":true}})
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            h.json("/api/notifications/unread", &bob).await,
            json!({"unread":false})
        );

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
