use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use uuid::Uuid;

use super::Server;
use crate::{
    app::{self, Error, Result},
    catalog::MembershipSnapshot,
    cells::{
        self, Address, CellClient,
        directory::{
            AccountsExist, IdentityKey, LookupAccounts,
            notifications::{
                Access, Deliver, Delivery, HandlesAt, MAX_RECIPIENTS, Notice, Reason, Recipient,
                ResolveHandlesAt,
            },
        },
        organization::teams::{MentionMembers, MentionMembersInput, MentionRoster, Page},
        repository::collaboration::{
            mention_settings::{self, Settings},
            mentions::{Acknowledge, Event, EventKey, Mention, Pending},
        },
    },
    repository::Repository,
};

const SWEEP_INTERVAL: Duration = Duration::from_secs(2);
const SWEEP_BATCH: usize = 16;

async fn current_grants(server: &Server, repo: &Repository) -> Result<Option<MembershipSnapshot>> {
    let Some(catalog) = server.catalog() else {
        return Ok(None);
    };
    match catalog
        .membership(&repo.config.owner, &repo.config.name)
        .await
    {
        Ok(snapshot) => Ok((snapshot.repository_id == repo.id).then_some(snapshot)),
        Err(crate::catalog::CatalogError::NotFound) => Ok(None),
        Err(error) => Err(Error::Membership(Box::new(error))),
    }
}

async fn account_ids(client: &CellClient, identities: &[IdentityKey]) -> Result<Vec<Option<Uuid>>> {
    let mut result = Vec::with_capacity(identities.len());
    for batch in identities.chunks(100) {
        let ids = app::query_output(
            client
                .query::<LookupAccounts>(Address::Directory, None, batch.to_vec())
                .await,
        )?;
        if ids.len() != batch.len() {
            return Err(Error::CellContract(
                "Notification account lookup returned an invalid count",
            ));
        }
        result.extend(ids);
    }
    Ok(result)
}

async fn raw_handles(client: &CellClient, event: &Event) -> Result<BTreeMap<String, Uuid>> {
    let names: Vec<_> = event
        .current
        .iter()
        .chain(&event.previous)
        .filter_map(|mention| match mention {
            Mention::Handle(handle) => Some(handle.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let result = app::query_output(
        client
            .query::<ResolveHandlesAt>(
                Address::Directory,
                None,
                HandlesAt {
                    names: names.clone(),
                    before_ms: event.created_at_ms,
                },
            )
            .await,
    )?;
    if names.len() != result.len() {
        return Err(Error::CellContract(
            "Notification handle lookup returned an invalid count",
        ));
    }
    Ok(names
        .into_iter()
        .zip(result)
        .filter_map(|(name, id)| Some((name, id?)))
        .collect())
}

type TeamKey = (Uuid, Uuid);

struct DeliveryScope {
    direct: BTreeSet<Uuid>,
    revision: u64,
    group_access: bool,
}

async fn team_members(
    client: &CellClient,
    event: &Event,
    settings: &Settings,
    sender: Option<Uuid>,
    candidates: Option<&[Uuid]>,
) -> Result<BTreeMap<TeamKey, Vec<Uuid>>> {
    let Some(sender) = sender else {
        return Ok(BTreeMap::new());
    };
    let teams: BTreeSet<_> = event
        .current
        .iter()
        .chain(&event.previous)
        .filter_map(|mention| {
            let Mention::Team { organization, team } = mention else {
                return None;
            };
            // An association removed between commit and processing cannot deliver
            // an old queued mention to a team no longer mentionable in this repo.
            (settings.organization == Some(*organization)
                && settings.teams.iter().any(|alias| alias.id == *team))
            .then_some((*organization, *team))
        })
        .collect();
    let mut result = BTreeMap::new();
    'teams: for (organization, team) in teams {
        let mut members = Vec::new();
        let mut batches = candidates.unwrap_or_default().chunks(100);
        let mut cursor = None;
        loop {
            let roster = if candidates.is_some() {
                let Some(batch) = batches.next() else { break };
                MentionRoster::Candidates(batch.to_vec())
            } else {
                MentionRoster::Page(cursor.take())
            };
            match client
                .query::<MentionMembers>(
                    Address::Organization(organization),
                    None,
                    MentionMembersInput {
                        actor: sender,
                        team,
                        roster,
                    },
                )
                .await
            {
                Ok(found) => match found.output {
                    Page::Changed => return Err(Error::PageChanged),
                    Page::Page { items, next, .. } => {
                        members.extend(items);
                        if candidates.is_none() && next.is_none() {
                            break;
                        }
                        cursor = next;
                    }
                },
                Err(cells::Error::AccessDenied | cells::Error::NotInitialized) => continue 'teams,
                Err(error) => return Err(Error::Cell(error)),
            }
        }
        result.insert((organization, team), members);
    }
    Ok(result)
}

fn expand(
    mentions: &[Mention],
    handles: &BTreeMap<String, Uuid>,
    teams: &BTreeMap<TeamKey, Vec<Uuid>>,
) -> BTreeMap<Uuid, Reason> {
    let mut expanded = BTreeMap::new();
    for mention in mentions {
        match mention {
            Mention::Actor(id) => {
                expanded.insert(*id, Reason::Mention);
            }
            Mention::Handle(handle) => {
                if let Some(id) = handles.get(handle) {
                    expanded.insert(*id, Reason::Mention);
                }
            }
            Mention::Team { organization, team } => {
                if let Some(actors) = teams.get(&(*organization, *team)) {
                    for actor in actors {
                        expanded.entry(*actor).or_insert(Reason::TeamMention);
                    }
                }
            }
        }
    }
    expanded
}

async fn deliver(
    server: &Server,
    repo: &Repository,
    client: &CellClient,
    scope: &DeliveryScope,
    settings: &Settings,
    event: &Event,
) -> Result<()> {
    let sender_key = IdentityKey {
        issuer: event.actor.issuer.clone(),
        subject: event.actor.subject.clone(),
    };
    let mut recipients = if !event.assignees.is_empty() {
        let sender = account_ids(client, std::slice::from_ref(&sender_key))
            .await?
            .into_iter()
            .next()
            .flatten();
        event
            .assignees
            .iter()
            .copied()
            .filter(|id| Some(*id) != sender)
            .map(|id| (id, Reason::Assigned))
            .collect()
    } else if event.reviewers.is_empty() {
        let (handles, sender) = tokio::try_join!(
            raw_handles(client, event),
            account_ids(client, std::slice::from_ref(&sender_key)),
        )?;
        let sender = sender.into_iter().next().flatten();
        let candidates: Vec<_> = scope.direct.iter().copied().collect();
        let teams = team_members(
            client,
            event,
            settings,
            sender,
            (!scope.group_access).then_some(candidates.as_slice()),
        )
        .await?;
        let previous = expand(&event.previous, &handles, &teams);
        let mut recipients = expand(&event.current, &handles, &teams);
        recipients.retain(|actor, _| Some(*actor) != sender && !previous.contains_key(actor));
        recipients
    } else {
        account_ids(client, &event.reviewers)
            .await?
            .into_iter()
            .flatten()
            .map(|id| (id, Reason::ReviewRequested))
            .collect()
    };
    recipients.retain(|actor, _| scope.group_access || scope.direct.contains(actor));
    if recipients.is_empty() {
        return Ok(());
    }
    let Some(fresh) = current_grants(server, repo).await? else {
        return Ok(());
    };
    if fresh.revision != scope.revision {
        // Expansion depends on whether there are group grants. Recompute after
        // a roster edit so a candidate isn't assigned to the wrong collection.
        return Err(Error::PageChanged);
    }
    let notice = Notice {
        id: event.id,
        repository: repo.id,
        target: event.target,
        created_at_ms: event.created_at_ms,
        sender: event.actor.name.clone(),
    };
    // Every recipient/event key is idempotent in Directory. A failed batch or
    // lost acknowledgement leaves this source event queued for another pass.
    let recipients: Vec<_> = recipients.into_iter().collect();
    for batch in recipients.chunks(MAX_RECIPIENTS) {
        let candidates: Vec<_> = batch
            .iter()
            .filter_map(|(actor, _)| (!scope.direct.contains(actor)).then_some(*actor))
            .collect();
        let expected = candidates.len();
        let existing = if candidates.is_empty() {
            Vec::new()
        } else {
            app::query_output(
                client
                    .query::<AccountsExist>(Address::Directory, None, candidates)
                    .await,
            )?
        };
        if existing.len() != expected {
            return Err(Error::CellContract(
                "Notification recipient lookup returned an invalid count",
            ));
        }
        let mut existing = existing.into_iter();
        let recipients: Vec<_> = batch
            .iter()
            .filter_map(|&(actor, reason)| {
                let access = if scope.direct.contains(&actor) {
                    Access::Direct
                } else if existing.next() == Some(true) {
                    Access::Group
                } else {
                    return None;
                };
                Some(Recipient {
                    actor,
                    reason,
                    access,
                })
            })
            .collect();
        if recipients.is_empty() {
            continue;
        }
        app::command_output(
            client
                .command::<Deliver>(
                    Address::Directory,
                    app::mutation_identity()?,
                    Delivery {
                        notice: notice.clone(),
                        recipients,
                    },
                )
                .await,
        )?;
    }
    Ok(())
}

pub(crate) async fn drain_repository(server: &Server, repo: &Repository) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    let events = app::query_output(cell.client.query::<Pending>(&cell.target, None, ()).await)?;
    if events.is_empty() {
        return Ok(());
    }
    let grants = current_grants(server, repo).await?;
    let roster = match (server.auth.as_ref(), grants.as_ref()) {
        (Some(auth), Some(snapshot)) => {
            let mut members = snapshot.members.clone();
            if let Some(access) = &snapshot.organization {
                members.extend(
                    crate::repository::access::organization_members(
                        server,
                        access.id,
                        &access.teams,
                        None,
                    )
                    .await?,
                );
            }
            let identities: Vec<_> = members
                .into_iter()
                .map(|member| IdentityKey {
                    issuer: auth.issuer().to_owned(),
                    subject: member.subject,
                })
                .collect();
            let ids = account_ids(&cell.client, &identities).await?;
            // Group candidates are only visible when the inbox principal has a
            // current verified repository grant. No background session is used.
            Some(DeliveryScope {
                direct: ids.into_iter().flatten().collect(),
                revision: snapshot.revision,
                group_access: auth.is_oidc() && !snapshot.groups.is_empty(),
            })
        }
        _ => None,
    };
    let settings = app::query_output(
        cell.client
            .query::<mention_settings::Get>(&cell.target, None, ())
            .await,
    )?;
    for event in events {
        if let Some(scope) = &roster
            && (!scope.direct.is_empty() || scope.group_access)
        {
            deliver(server, repo, &cell.client, scope, &settings, &event).await?;
        }
        app::command_output(
            cell.client
                .command::<Acknowledge>(
                    &cell.target,
                    app::mutation_identity()?,
                    EventKey {
                        id: event.id,
                        created_at_ms: event.created_at_ms,
                    },
                )
                .await,
        )?;
    }
    Ok(())
}

pub(super) async fn sweep(server: Arc<Server>) {
    let mut cursor = 0;
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
        let repositories = server.repositories.values();
        if repositories.is_empty() || server.cells().is_none() {
            continue;
        }
        let count = repositories.len().min(SWEEP_BATCH);
        for offset in 0..count {
            let repo = &repositories[(cursor + offset) % repositories.len()];
            tokio::select! {
                () = server.cancellation.cancelled() => return,
                result = drain_repository(&server, repo) => {
                    if let Err(error) = result {
                        tracing::warn!(repository_id = %repo.id, %error, "mention notification delivery will be retried");
                    }
                }
            }
        }
        cursor = (cursor + count) % repositories.len();
    }
}
#[cfg(test)]
mod tests;
