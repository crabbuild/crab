use std::collections::BTreeMap;

use axum::extract::Query;
use serde::Deserialize;

use super::*;
use crate::{
    cells::repository::{
        collaboration::{Subject, SubjectKind},
        webhooks as hooks,
    },
    server::webhooks,
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/settings/webhooks",
        get(settings)
            .post(change)
            .layer(DefaultBodyLimit::max(2048)),
    )
}

fn destinations<'a>(
    server: &'a Server,
    repo: &Repository,
) -> Option<&'a BTreeMap<String, url::Url>> {
    server.webhook_sender.as_ref()?.destinations(repo.id)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    destination: Option<String>,
}

async fn settings(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(selection): Query<Selection>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let snapshot = app::query_output(
        cell.client
            .query::<hooks::View>(cell.target, None, ())
            .await,
    )?;
    require_admin(&server, &principal, &repo).await?;
    let approved = destinations(&server, &repo);
    let mut receivers: BTreeMap<_, _> = approved
        .into_iter()
        .flat_map(|map| map.iter())
        .map(|(id, url)| (id.clone(), Some(url)))
        .collect();
    for id in snapshot
        .subscriptions
        .iter()
        .map(|hook| &hook.destination)
        .chain(
            snapshot
                .deliveries
                .iter()
                .map(|delivery| &delivery.subscription.destination),
        )
    {
        receivers.entry(id.clone()).or_insert(None);
    }
    let selected = selection
        .destination
        .filter(|id| receivers.contains_key(id))
        .or_else(|| approved.and_then(|map| map.keys().next().cloned()))
        .or_else(|| receivers.keys().next().cloned());
    let target = selected
        .as_ref()
        .and_then(|id| receivers.get(id).copied().flatten());
    let current = snapshot
        .subscriptions
        .iter()
        .find(|hook| Some(&hook.destination) == selected.as_ref());
    let active = current.is_some() && target.is_some();
    let deliveries = snapshot.deliveries.into_iter()
        .filter(|delivery| Some(&delivery.subscription.destination) == selected.as_ref())
        .map(|delivery| {
        use hooks::Event;
        let event = delivery.event.name();
        let (action, subject, comment, head_change) = match delivery.event {
            Event::Push { .. } => (Value::Null, None, None, None),
            Event::PullRequest { action, number, head_change, .. } => {
                (json!(action), Some(Subject {kind: SubjectKind::Pull, number}), None, head_change)
            }
            Event::Issues { action, number, .. } => (
                json!(action), Some(Subject {kind: SubjectKind::Issue, number}), None, None,
            ),
            Event::IssueComment { action, subject, comment, .. } => (
                json!(action), Some(subject), Some(comment), None,
            ),
        };
        let issue = subject.filter(|value| value.kind == SubjectKind::Issue).map(|value| value.number);
        let pull = subject.filter(|value| value.kind == SubjectKind::Pull).map(|value| value.number);
        json!({
            "id":delivery.id,"event":event,"action":action,"issue":issue,"pull":pull,"comment":comment,"head_change":head_change,"queued_at":delivery.queued_at,
            "status":delivery.status,"attempts":delivery.attempts,"http_status":delivery.http_status,
            "version":delivery.version,"recent_attempts":delivery.recent_attempts,
            "can_redeliver":active && current == Some(&delivery.subscription) && matches!(delivery.status, hooks::Status::Failed | hooks::Status::Delivered),
        })
    }).collect::<Vec<_>>();
    let subscription = current.map(|hook| {
        json!({
            "id":hook.id,"push":hook.push,"pull_request":hook.pull_request,
            "issues":hook.issues,"issue_comment":hook.issue_comment,
        })
    });
    let receivers: Vec<_> = receivers
        .into_iter()
        .map(|(id, url)| json!({"id":id,"url":url}))
        .collect();
    Ok(Json(
        json!({"destinations":receivers,"selected":selected,"destination":target,"subscription":subscription,"active":active,"deliveries":deliveries}),
    ))
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    Configure {
        expected: Option<Uuid>,
        id: Uuid,
        push: bool,
        pull_request: bool,
        #[serde(default)]
        issues: bool,
        #[serde(default)]
        issue_comment: bool,
    },
    Disable {
        expected: Uuid,
    },
    Redeliver {
        id: Uuid,
        version: u64,
    },
}

async fn require_admin(server: &Server, principal: &Principal, repo: &Repository) -> Result<()> {
    if !app::membership(server, principal, repo)
        .await?
        .principal
        .can_admin(repo.id)
    {
        return Err(Error::NotFound);
    }
    Ok(())
}

async fn change(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(selection): Query<Selection>,
    Json(input): Json<Change>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    require_admin(&server, &principal, &repo).await?;
    let cell = app::repository_cell(&server, &repo)?;
    let approved = destinations(&server, &repo);
    let destination = match selection.destination {
        Some(id) if blake3::Hash::from_hex(&id).is_ok() => id,
        Some(_) => return Err(Error::Invalid("Invalid webhook destination")),
        None => {
            let destinations = approved.ok_or(Error::NotFound)?;
            if destinations.len() != 1 {
                return Err(Error::Invalid("Select a webhook destination"));
            }
            destinations.keys().next().cloned().ok_or(Error::NotFound)?
        }
    };
    if let Change::Redeliver { id, version } = input {
        if !approved.is_some_and(|map| map.contains_key(&destination)) {
            return Err(Error::NotFound);
        }
        let saved = app::command_output(
            cell.client
                .command::<hooks::Retry>(
                    cell.target,
                    app::mutation_identity()?,
                    (id, version, destination),
                )
                .await,
        )?;
        return if saved {
            Ok(Json(json!({"queued":true})))
        } else {
            Err(Error::PageChanged)
        };
    }
    let (expected, subscription, secret) = match input {
        Change::Configure {
            expected,
            id,
            push,
            pull_request,
            issues,
            issue_comment,
        } => {
            if !approved.is_some_and(|map| map.contains_key(&destination)) {
                return Err(Error::NotFound);
            }
            if !(push || pull_request || issues || issue_comment) || id.is_nil() {
                return Err(Error::Invalid("Choose at least one webhook event"));
            }
            let secret = webhooks::secret(&server, repo.id, id).ok_or(Error::NotFound)?;
            (
                expected,
                Some(hooks::Subscription {
                    id,
                    destination: destination.clone(),
                    push,
                    pull_request,
                    issues,
                    issue_comment,
                }),
                Some(secret),
            )
        }
        Change::Disable { expected } => (Some(expected), None, None),
        Change::Redeliver { .. } => return Err(Error::PageChanged),
    };
    let saved = app::command_output(
        cell.client
            .command::<hooks::Configure>(
                cell.target,
                app::mutation_identity()?,
                hooks::ConfigureInput {
                    destination,
                    expected,
                    subscription,
                },
            )
            .await,
    )?;
    if !saved {
        return Err(Error::PageChanged);
    }
    Ok(Json(json!({"secret":secret})))
}
