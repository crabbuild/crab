use serde::Deserialize;

use super::*;
use crate::{cells::repository::webhooks as hooks, server::webhooks};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/settings/webhooks",
        get(settings)
            .post(change)
            .layer(DefaultBodyLimit::max(2048)),
    )
}

fn target<'a>(server: &'a Server, repo: &Repository) -> Option<&'a url::Url> {
    server.webhook_sender.as_ref()?.target(repo.id)
}

async fn settings(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    let cell = app::repository_cell(&server, &repo)?;
    let snapshot = app::query_output(
        cell.client
            .query::<hooks::View>(cell.target, None, ())
            .await,
    )?;
    require_admin(&server, &principal, &repo).await?;
    let target = target(&server, &repo);
    let binding = target.map(webhooks::destination);
    let current = snapshot.subscription.as_ref();
    let active = current.is_some_and(|hook| Some(&hook.destination) == binding.as_ref());
    let deliveries = snapshot.deliveries.into_iter().map(|delivery| {
        use hooks::Event;
        let (event, action, pull, head_change) = match delivery.event {
            Event::Push { .. } => ("push", None, None, None),
            Event::PullRequest { action, number, head_change, .. } => {
                ("pull_request", Some(action), Some(number), head_change)
            }
        };
        json!({
            "id":delivery.id,"event":event,"action":action,"pull":pull,"head_change":head_change,"queued_at":delivery.queued_at,
            "status":delivery.status,"attempts":delivery.attempts,"http_status":delivery.http_status,
            "version":delivery.version,"recent_attempts":delivery.recent_attempts,
            "can_redeliver":active && current == Some(&delivery.subscription) && matches!(delivery.status, hooks::Status::Failed | hooks::Status::Delivered),
        })
    }).collect::<Vec<_>>();
    let subscription =
        current.map(|hook| json!({"id":hook.id,"push":hook.push,"pull_request":hook.pull_request}));
    Ok(Json(
        json!({"destination":target,"subscription":subscription,"active":active,"deliveries":deliveries}),
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
    Json(input): Json<Change>,
) -> Result<Json<Value>> {
    let repo = admin(&server, &principal, &key)?;
    require_admin(&server, &principal, &repo).await?;
    let cell = app::repository_cell(&server, &repo)?;
    if let Change::Redeliver { id, version } = input {
        let url = target(&server, &repo).ok_or(Error::NotFound)?;
        let saved = app::command_output(
            cell.client
                .command::<hooks::Retry>(
                    cell.target,
                    app::mutation_identity()?,
                    (id, version, webhooks::destination(url)),
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
        } => {
            let receiver = target(&server, &repo).ok_or(Error::NotFound)?;
            if !(push || pull_request) || id.is_nil() {
                return Err(Error::Invalid("Choose at least one webhook event"));
            }
            let secret = webhooks::secret(&server, repo.id, id).ok_or(Error::NotFound)?;
            (
                expected,
                Some(hooks::Subscription {
                    id,
                    destination: webhooks::destination(receiver),
                    push,
                    pull_request,
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
