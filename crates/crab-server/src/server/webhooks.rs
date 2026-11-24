use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crab_git::receive_plan::RefUpdate;
use crab_metadata::capsule_protocol::{CapsulePlanReceipt, resolve_capsule_plan_receipt};
use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use url::Url;
use uuid::Uuid;

use crate::{
    WebhookDestination,
    app::{self, Error, Result},
    cells::repository::webhooks::{
        self as stored, Delivery, Event, FinishInput, HeadChange, Outcome, sync,
    },
    repository::Repository,
    server::Server,
};

pub(crate) struct Sender {
    client: reqwest::Client,
    targets: BTreeMap<Uuid, BTreeMap<String, Url>>,
}

impl Sender {
    pub(crate) fn new(config: &[WebhookDestination]) -> crate::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        let mut targets = BTreeMap::<_, BTreeMap<_, _>>::new();
        for entry in config {
            targets
                .entry(entry.repository)
                .or_default()
                .insert(destination(&entry.url), entry.url.clone());
        }
        Ok(Self { client, targets })
    }

    pub(crate) fn destinations(&self, repository: Uuid) -> Option<&BTreeMap<String, Url>> {
        self.targets.get(&repository)
    }

    pub(crate) fn target(&self, repository: Uuid, key: &str) -> Option<&Url> {
        self.destinations(repository)?.get(key)
    }

    async fn send(
        &self,
        target: &Url,
        id: Uuid,
        event: &str,
        secret: &str,
        payload: Value,
    ) -> Result<Outcome> {
        let body = serde_json::to_vec(&payload).map_err(Error::Json)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
            .map_err(|_| Error::CellContract("Invalid outbound webhook secret"))?;
        mac.update(&body);
        let digest = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
        let response = self
            .client
            .post(target.clone())
            .header("content-type", "application/json")
            .header("x-crab-event", event)
            .header("x-crab-delivery", id.to_string())
            .header("x-hub-signature-256", format!("sha256={digest}"))
            .body(body)
            .send()
            .await;
        Ok(match response {
            Ok(reply) if reply.status().is_success() => Outcome::Delivered(reply.status().as_u16()),
            Ok(reply) => Outcome::Failed(Some(reply.status().as_u16())),
            Err(_) => Outcome::Failed(None),
        })
    }
}

pub(crate) fn destination(url: &Url) -> String {
    blake3::hash(url.as_str().as_bytes()).to_hex().to_string()
}

pub(crate) fn secret(server: &Server, repo: Uuid, id: Uuid) -> Option<String> {
    let key = server.auth.as_ref()?.webhook_key()?;
    let mut hash = blake3::Hasher::new_keyed(key);
    hash.update(b"crab/outbound-webhook/v1");
    hash.update(repo.as_bytes());
    hash.update(id.as_bytes());
    Some(hash.finalize().to_hex().to_string())
}

pub(crate) fn is_push_ref(name: &str) -> bool {
    name.starts_with("refs/heads/") || name.starts_with("refs/tags/")
}

pub(crate) async fn reserve_push(server: &Server, repo: &Repository, plan: &str) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<stored::QueuePush>(cell.target, app::mutation_identity()?, plan.to_owned())
            .await,
    )
}

fn head_changed(update: &RefUpdate) -> bool {
    update.name.starts_with("refs/heads/") && update.new.is_some() && update.old != update.new
}

pub(crate) fn observes_publication(
    server: &Server,
    repo: &Repository,
    updates: &[RefUpdate],
) -> bool {
    let Some(sender) = &server.webhook_sender else {
        return false;
    };
    let local = sender.destinations(repo.id).is_some();
    let upstream = repo
        .forked_from
        .is_some_and(|id| sender.destinations(id).is_some());
    updates.iter().any(|update| {
        (local && update.old != update.new && is_push_ref(&update.name))
            || (upstream && head_changed(update))
    })
}

pub(crate) async fn reserve_publication(
    server: &Server,
    repo: &Repository,
    plan: &str,
    updates: &[RefUpdate],
) -> Result<()> {
    let Some(sender) = &server.webhook_sender else {
        return Ok(());
    };
    let synchronize = updates.iter().any(head_changed);
    if let Some(destinations) = sender.destinations(repo.id) {
        reserve_push(server, repo, plan).await?;
        if synchronize {
            reserve_sync(server, repo, None, plan, destinations).await?;
        }
    }
    if synchronize
        && let Some(upstream) = repo.fork_source(server)
        && let Some(destinations) = sender.destinations(upstream.id)
    {
        reserve_sync(server, &upstream, Some(repo.id), plan, destinations).await?;
    }
    Ok(())
}

async fn reserve_sync(
    server: &Server,
    repo: &Repository,
    source: Option<Uuid>,
    plan: &str,
    destinations: &BTreeMap<String, Url>,
) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<sync::Reserve>(
                cell.target,
                app::mutation_identity()?,
                sync::Reservation {
                    plan: plan.to_owned(),
                    source,
                    destinations: destinations.keys().cloned().collect(),
                },
            )
            .await,
    )
}

async fn committed_plan(repo: &Repository, plan: &str) -> Option<CapsulePlanReceipt> {
    match resolve_capsule_plan_receipt(&repo.store, &repo.layout, plan).await {
        Ok(receipt) => receipt,
        Err(error) => {
            tracing::warn!(repository = %repo.id, %error, "webhook ref publication receipt is unavailable");
            None
        }
    }
}

async fn resolve_sync(
    server: &Server,
    sender: &Sender,
    repo: &Repository,
    pending: &sync::Pending,
) -> sync::Resolution {
    if sender.target(repo.id, &pending.destination).is_none() {
        return sync::Resolution::Discard;
    }
    let fork = if let Some(id) = pending.source {
        let Some(fork) = server.repositories.by_id(id) else {
            return sync::Resolution::Wait;
        };
        if !fork
            .fork_source(server)
            .is_some_and(|source| source.id == repo.id)
        {
            return sync::Resolution::Discard;
        }
        Some(fork)
    } else {
        None
    };
    let source = fork.as_deref().unwrap_or(repo);
    let Some(receipt) = committed_plan(source, &pending.plan).await else {
        return sync::Resolution::Wait;
    };
    let changes = receipt
        .transaction()
        .edits()
        .iter()
        .filter_map(|edit| {
            let after = edit.new_oid()?;
            let before = edit.expected_old();
            (pending.heads.contains(edit.ref_name())
                && edit.ref_name().starts_with("refs/heads/")
                && before != Some(after))
            .then(|| {
                (
                    edit.ref_name().to_owned(),
                    HeadChange {
                        before: before.map(str::to_owned),
                        after: after.to_owned(),
                    },
                )
            })
        })
        .collect();
    sync::Resolution::Committed(changes)
}

async fn deliver(
    server: &Server,
    sender: &Sender,
    repo: &Repository,
    delivery: &Delivery,
) -> Result<Outcome> {
    let Some(url) = sender.target(repo.id, &delivery.subscription.destination) else {
        return Ok(Outcome::Cancelled);
    };
    let Some(secret) = secret(server, repo.id, delivery.subscription.id) else {
        return Ok(Outcome::Cancelled);
    };
    let (name, event) = match &delivery.event {
        Event::Push { plan } => {
            let Some(receipt) = committed_plan(repo, plan).await else {
                return Ok(Outcome::WaitingForPublication);
            };
            let updates = receipt.transaction().edits().iter()
                .filter(|edit| edit.expected_old() != edit.new_oid() && is_push_ref(edit.ref_name()))
                .map(|edit| json!({"ref":edit.ref_name(),"before":edit.expected_old(),"after":edit.new_oid()}))
                .collect::<Vec<_>>();
            if updates.is_empty() {
                return Ok(Outcome::Cancelled);
            }
            ("push", json!({"updates":updates}))
        }
        event @ Event::PullRequest { .. } => (
            "pull_request",
            serde_json::to_value(event).map_err(Error::Json)?,
        ),
    };
    let cell = app::repository_cell(server, repo)?;
    let active = app::query_output(
        cell.client
            .query::<stored::Current>(cell.target, None, delivery.subscription.clone())
            .await,
    )?;
    if !active {
        return Ok(Outcome::Cancelled);
    }
    let body = json!({"repository":repo.id,"delivery":delivery.id,"occurred_at_ms":delivery.queued_at,"data":event});
    sender.send(url, delivery.id, name, &secret, body).await
}

pub(crate) async fn drain(server: &Server, repo: &Repository, sender: &Sender) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    for _ in 0..8 {
        if server.cancellation.is_cancelled() {
            return Ok(());
        }
        let Some(pending) =
            app::query_output(cell.client.query::<sync::Next>(cell.target, None, ()).await)?
        else {
            break;
        };
        let resolution = resolve_sync(server, sender, repo, &pending).await;
        app::command_output(
            cell.client
                .command::<sync::Advance>(
                    cell.target,
                    app::mutation_identity()?,
                    sync::Step {
                        id: pending.id,
                        due: pending.due,
                        cursor: pending.cursor,
                        through: pending.through,
                        resolution,
                    },
                )
                .await,
        )?;
    }
    for _ in 0..8 {
        if server.cancellation.is_cancelled()
            || !app::query_output(
                cell.client
                    .query::<stored::HasWork>(cell.target, None, ())
                    .await,
            )?
        {
            break;
        }
        let claimed = app::command_output(
            cell.client
                .command::<stored::Claim>(cell.target, app::mutation_identity()?, ())
                .await,
        )?;
        let Some(delivery) = claimed else {
            continue;
        };
        if server.cancellation.is_cancelled() {
            break;
        }
        let outcome = deliver(server, sender, repo, &delivery).await?;
        app::command_output(
            cell.client
                .command::<stored::Finish>(
                    cell.target,
                    app::mutation_identity()?,
                    FinishInput {
                        id: delivery.id,
                        version: delivery.version,
                        outcome,
                    },
                )
                .await,
        )?;
    }
    Ok(())
}

pub(crate) async fn run(server: Arc<Server>) -> crate::Result<()> {
    let Some(sender) = &server.webhook_sender else {
        return Ok(());
    };
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let server = server.as_ref();
        let targets = sender
            .targets
            .keys()
            .filter_map(|id| server.repositories.by_id(*id))
            .collect::<Vec<_>>();
        let sweep = stream::iter(targets).for_each_concurrent(8, |repo| async move {
            if let Err(error) = drain(server, &repo, sender).await {
                tracing::warn!(repository = %repo.id, %error, "outbound webhook queue will retry");
            }
        });
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            _ = sweep => {}
        }
    }
}
