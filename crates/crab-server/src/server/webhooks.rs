use std::{collections::BTreeMap, sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use url::Url;
use uuid::Uuid;

use crate::{
    WebhookDestination,
    app::{self, Error, Result},
    cells::repository::webhooks::{self as stored, Delivery, Event, FinishInput, Outcome},
    repository::Repository,
    server::Server,
};

pub(crate) struct Sender {
    client: reqwest::Client,
    targets: BTreeMap<Uuid, Url>,
}

impl Sender {
    pub(crate) fn new(config: &[WebhookDestination]) -> crate::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        Ok(Self {
            client,
            targets: config
                .iter()
                .map(|entry| (entry.repository, entry.url.clone()))
                .collect(),
        })
    }

    pub(crate) fn target(&self, repository: Uuid) -> Option<&Url> {
        self.targets.get(&repository)
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

pub(crate) async fn reserve_push(server: &Server, repo: &Repository, plan: &str) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    app::command_output(
        cell.client
            .command::<stored::QueuePush>(cell.target, app::mutation_identity()?, plan.to_owned())
            .await,
    )
}

async fn deliver(
    server: &Server,
    sender: &Sender,
    repo: &Repository,
    delivery: &Delivery,
) -> Result<Outcome> {
    let Some(url) = sender
        .target(repo.id)
        .filter(|url| destination(url) == delivery.subscription.destination)
    else {
        return Ok(Outcome::Cancelled);
    };
    let Some(secret) = secret(server, repo.id, delivery.subscription.id) else {
        return Ok(Outcome::Cancelled);
    };
    let (name, event) = match &delivery.event {
        Event::Push { plan } => {
            let receipt = match crab_metadata::capsule_protocol::resolve_capsule_plan_receipt(
                &repo.store,
                &repo.layout,
                plan,
            )
            .await
            {
                Ok(Some(receipt)) => receipt,
                Ok(None) => return Ok(Outcome::WaitingForPublication),
                Err(error) => {
                    tracing::warn!(repository = %repo.id, %error, "webhook ref publication receipt is unavailable");
                    return Ok(Outcome::WaitingForPublication);
                }
            };
            let updates = receipt.transaction().edits().iter()
                .filter(|edit| edit.expected_old() != edit.new_oid() && (edit.ref_name().starts_with("refs/heads/") || edit.ref_name().starts_with("refs/tags/")))
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
            .query::<stored::Current>(cell.target, None, ())
            .await,
    )?;
    if active.as_ref() != Some(&delivery.subscription) {
        return Ok(Outcome::Cancelled);
    }
    let body = json!({"repository":repo.id,"delivery":delivery.id,"occurred_at_ms":delivery.queued_at,"data":event});
    sender.send(url, delivery.id, name, &secret, body).await
}

pub(crate) async fn drain(server: &Server, repo: &Repository, sender: &Sender) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
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
