use std::{sync::Arc, time::Duration};

use reqwest::{Client, StatusCode, header, redirect::Policy};
use serde::Serialize;
use url::Url;

use super::{Server, mentions::current_grants};
use crate::{
    NotificationEmailConfig,
    app::{self, Error},
    cells::{
        Address, CellClient,
        directory::{
            LookupIdentities,
            notifications::email::{
                ClaimMail, FinishMail, Job, MailAttempt, MailDestination, MailFinish, MailPending,
            },
        },
        repository::collaboration::watches,
    },
};

#[derive(Debug, thiserror::Error)]
pub(super) enum DeliveryError {
    #[error(transparent)]
    Queue(#[from] Error),
    #[error("Notification account authority is unavailable")]
    Account(#[from] crate::auth::AuthError),
}

type Result<T> = std::result::Result<T, DeliveryError>;

mod transfers;

const IDLE: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_MS: u64 = 60_000;

pub(crate) struct Sender {
    client: Client,
    endpoint: Url,
    key: String,
    from: String,
    text: String,
    transfer_text: String,
}

#[derive(Serialize)]
struct Message<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'static str,
    text: &'a str,
}

impl Sender {
    pub(super) fn new(settings: &NotificationEmailConfig, public: &Url) -> crate::Result<Self> {
        let key = std::fs::read_to_string(&settings.api_key_file)?;
        let endpoint = Url::parse("https://api.resend.com/emails")
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        Self::with_endpoint(
            settings.from.clone(),
            key.trim().to_owned(),
            public,
            endpoint,
        )
    }

    pub(crate) fn with_endpoint(
        from: String,
        key: String,
        public: &Url,
        endpoint: Url,
    ) -> crate::Result<Self> {
        if key.is_empty() || key.len() > 1024 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(crate::Error::Config(
                "notification email API-key file is invalid",
            ));
        }
        let client = Client::builder()
            .timeout(TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        let inbox = public
            .join("/-/notifications")
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        let transfers = public
            .join("/-/notifications?tab=transfers")
            .map_err(|error| crate::Error::Maintenance(Box::new(error)))?;
        Ok(Self {
            client,
            endpoint,
            key,
            from,
            // Email can outlive repository access. Keep discussion content in
            // the authenticated inbox, where access is checked on each read.
            text: format!("You have new activity in Crab.\n\nOpen your notifications: {inbox}\n"),
            transfer_text: format!(
                "You have a repository transfer offer in Crab.\n\nOpen your notifications: {transfers}\n"
            ),
        })
    }

    async fn send(&self, job: &Job, to: &str) -> Option<u64> {
        let (subject, text) = if job.transfer.is_some() {
            ("New repository transfer in Crab", &self.transfer_text)
        } else {
            ("New activity in Crab", &self.text)
        };
        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.key)
            .header("Idempotency-Key", job.id.provider_key())
            .json(&Message {
                from: &self.from,
                to: [to],
                subject,
                text,
            })
            .send()
            .await;
        let Ok(response) = response else {
            tracing::warn!("Notification email request failed");
            return Some(RETRY_MS);
        };
        let status = response.status();
        if status.is_success() {
            return None;
        }

        tracing::warn!(%status, "Notification email provider rejected the request");
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY {
            return None;
        }
        // Reusing the durable key and identical payload handles a lost reply
        // and a concurrent request for the same key at the provider.
        let retry_ms = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1_000));
        Some(retry_ms.unwrap_or(RETRY_MS))
    }
}

pub(super) async fn run(server: Arc<Server>) -> crate::Result<()> {
    let Some(sender) = &server.email else {
        return Ok(());
    };
    let cells = server.cells().ok_or(crate::cells::Error::Unavailable)?;
    tokio::join!(
        run_delivery(&server, &cells, sender),
        transfers::run(&server, &cells),
    );
    Ok(())
}

async fn run_delivery(server: &Server, cells: &CellClient, sender: &Sender) {
    let mut delay = IDLE;
    loop {
        tokio::select! {
            _ = server.cancellation.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        let result = tokio::select! {
            _ = server.cancellation.cancelled() => return,
            result = deliver_next(server, cells, sender) => result,
        };
        delay = match result {
            Ok(true) => Duration::from_millis(200),
            Ok(false) => IDLE,
            Err(error) => {
                tracing::warn!(%error, "Notification email queue is temporarily unavailable");
                IDLE
            }
        };
    }
}

pub(super) async fn deliver_next(
    server: &Server,
    cells: &CellClient,
    sender: &Sender,
) -> Result<bool> {
    if !app::query_output(
        cells
            .query::<MailPending>(Address::Directory, None, ())
            .await,
    )? {
        return Ok(false);
    }
    let Some(job) = app::command_output(
        cells
            .command::<ClaimMail>(Address::Directory, app::mutation_identity()?, ())
            .await,
    )?
    else {
        return Ok(false);
    };

    let attempt = MailAttempt {
        id: job.id,
        attempt: job.attempt,
    };
    let retry_after_ms = if can_deliver(server, cells, &job).await? {
        let address = app::query_output(
            cells
                .query::<MailDestination>(Address::Directory, None, attempt)
                .await,
        )?;
        if let Some(address) = address {
            sender.send(&job, &address).await
        } else {
            None
        }
    } else {
        None
    };
    app::command_output(
        cells
            .command::<FinishMail>(
                Address::Directory,
                app::mutation_identity()?,
                MailFinish {
                    attempt,
                    retry_after_ms,
                },
            )
            .await,
    )?;
    Ok(true)
}

async fn can_deliver(server: &Server, cells: &CellClient, job: &Job) -> Result<bool> {
    let Some(auth) = server.auth.as_ref() else {
        return Ok(false);
    };
    let identities = app::query_output(
        cells
            .query::<LookupIdentities>(Address::Directory, None, vec![job.id.actor])
            .await,
    )?;
    let [identity] = identities.as_slice() else {
        return Err(Error::CellContract("Identity lookup returned an invalid result count").into());
    };
    let Some(identity) = identity
        .as_ref()
        .filter(|identity| identity.issuer == auth.issuer())
    else {
        return Ok(false);
    };
    if let Some(source) = job.transfer {
        return Ok(!auth.account_state(&identity.subject).await?.suspended
            && transfers::still_pending(server, job.repository, source, identity).await?);
    }
    let Some(repo) = server.repositories.by_id(job.repository) else {
        return Ok(false);
    };
    let Some(grants) = current_grants(server, &repo).await? else {
        return Ok(false);
    };
    let direct = grants
        .members
        .iter()
        .any(|member| member.subject == identity.subject);
    if !direct {
        let team = if let Some(organization) = &grants.organization {
            match app::repository_team_access(server, organization, Some(job.id.actor), false).await
            {
                Ok(access) => access.owner || !access.teams.is_empty(),
                Err(Error::Cell(
                    crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied,
                )) => false,
                Err(error) => return Err(error.into()),
            }
        } else {
            false
        };
        if !team
            && (!auth.is_oidc()
                || grants.groups.is_empty()
                || auth
                    .mention_group(&identity.subject, &grants.groups)
                    .await
                    .map_err(Error::from)?
                    .is_none())
        {
            return Ok(false);
        }
    }
    let preference = app::query_output(
        cells
            .query::<watches::Get>(Address::Repository(repo.id), None, identity.clone())
            .await,
    )?;
    if preference.mode == watches::Mode::Muted {
        return Ok(false);
    }
    let state = auth.account_state(&identity.subject).await?;
    Ok(!state.suspended)
}

#[cfg(test)]
mod tests;
