use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use futures_util::{StreamExt, TryStreamExt, stream};
use uuid::Uuid;

use super::Server;
use crate::{
    RepositoryGroup,
    app::{self, Error, Result},
    catalog::MembershipSnapshot,
    cells::{
        self, Address, CellClient,
        directory::{
            IdentityKey, LookupAccounts, LookupActors, LookupIdentities, RegisterAccounts,
            notifications::{
                Access, Deliver, Delivery, HandlesAt, MAX_RECIPIENTS, Notice, NoticeTarget, Reason,
                Recipient, ReleaseKind, ResolveHandlesAt,
            },
        },
        organization::teams::{MentionMembers, MentionMembersInput, MentionRoster, Page},
        repository::collaboration::{
            mention_settings::{self, Settings},
            mentions::{
                Acknowledge, Enroll, Enrollment, Event, EventKey, FollowSource, Mention, Pending,
            },
            watches,
        },
    },
    repository::{Repository, access},
};

const SWEEP_INTERVAL: Duration = Duration::from_secs(2);
const SWEEP_BATCH: usize = 16;

pub(super) async fn current_grants(
    server: &Server,
    repo: &Repository,
) -> Result<Option<MembershipSnapshot>> {
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

pub(super) async fn recipient_has_grant(
    server: &Server,
    grants: &MembershipSnapshot,
    actor: Uuid,
    identity: &IdentityKey,
) -> Result<bool> {
    let Some(auth) = server
        .auth
        .as_ref()
        .filter(|auth| auth.issuer() == identity.issuer)
    else {
        return Ok(false);
    };
    if grants
        .members
        .iter()
        .any(|member| member.subject == identity.subject)
    {
        return Ok(true);
    }
    if let Some(organization) = &grants.organization {
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        let grant = access::organization_grant(
            &cells,
            organization.id,
            actor,
            !organization.teams.is_empty(),
        )
        .await?;
        if access::organization_access(&grant, &organization.teams, grants.baseline).is_some() {
            return Ok(true);
        }
    }
    if let Some(destination) = grants.fork_destination {
        let cells = server.cells().ok_or(Error::CellUnavailable)?;
        if access::organization_grant(&cells, destination, actor, false)
            .await?
            .owner
        {
            return Ok(true);
        }
    }
    if let Some(creator) = &grants.fork_creator
        && creator.actor.issuer == identity.issuer
        && creator.actor.subject == identity.subject
        && let Some(grant) = fork_creator_delivery(server, grants).await?
        && grant.actor == actor
        && (grant.access == Access::Direct
            || auth
                .mention_group(&identity.subject, &grant.groups)
                .await?
                .is_some())
    {
        return Ok(true);
    }
    Ok(auth.is_oidc()
        && !grants.groups.is_empty()
        && auth
            .mention_group(&identity.subject, &grants.groups)
            .await?
            .is_some())
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
    issuer: String,
    subjects: BTreeSet<String>,
    direct: BTreeSet<Uuid>,
    revision: u64,
    groups: Vec<RepositoryGroup>,
    creator: Option<ForkCreatorDelivery>,
}

#[derive(PartialEq, Eq)]
struct ForkCreatorDelivery {
    identity: IdentityKey,
    actor: Uuid,
    access: Access,
    groups: Vec<RepositoryGroup>,
}

async fn fork_creator_delivery(
    server: &Server,
    snapshot: &MembershipSnapshot,
) -> Result<Option<ForkCreatorDelivery>> {
    use access::ForkCreatorGrant;

    let (Some(source), Some(creator)) = (&snapshot.organization, &snapshot.fork_creator) else {
        return Ok(None);
    };
    let Some((actor, grant)) = access::fork_creator_grant(
        server,
        creator,
        source.id,
        &source.teams,
        snapshot.fork_destination,
    )
    .await?
    else {
        return Ok(None);
    };
    let (access, groups) = match grant {
        ForkCreatorGrant::Durable => (Access::Direct, Vec::new()),
        ForkCreatorGrant::Group => (Access::Group, creator.source_groups.clone()),
    };
    Ok(Some(ForkCreatorDelivery {
        identity: IdentityKey {
            issuer: creator.actor.issuer.clone(),
            subject: creator.actor.subject.clone(),
        },
        actor,
        access,
        groups,
    }))
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

impl DeliveryScope {
    fn access(&self, actor: &IdentityKey) -> Option<Access> {
        if actor.issuer != self.issuer {
            None
        } else if self.subjects.contains(&actor.subject) {
            Some(Access::Direct)
        } else if let Some(creator) = &self.creator
            && &creator.identity == actor
        {
            Some(creator.access)
        } else {
            (!self.groups.is_empty()).then_some(Access::Group)
        }
    }

    fn group_grants(&self, identity: &IdentityKey) -> &[RepositoryGroup] {
        // An upstream group can qualify only the recorded fork creator here.
        match &self.creator {
            Some(creator) if &creator.identity == identity && creator.access == Access::Group => {
                &creator.groups
            }
            _ => &self.groups,
        }
    }

    async fn current(&self, server: &Server, repo: &Repository) -> Result<bool> {
        let Some(fresh) = current_grants(server, repo).await? else {
            return Ok(false);
        };
        if fresh.revision != self.revision {
            // A roster change can move an account between inbox retention
            // classes. Resolve the delivery again against the new grant.
            return Err(Error::PageChanged);
        }
        // Organization policy and team changes can occur without a catalog write.
        if fresh.fork_creator.is_some()
            && fork_creator_delivery(server, &fresh).await? != self.creator
        {
            return Err(Error::PageChanged);
        }
        Ok(true)
    }
}

async fn publish(
    server: &Server,
    client: &CellClient,
    notice: &Notice,
    recipients: Vec<Recipient>,
) -> Result<()> {
    if recipients.is_empty() {
        return Ok(());
    }
    // Directory deduplicates the event per account before acknowledging it.
    app::command_output(
        client
            .command::<Deliver>(
                Address::Directory,
                app::mutation_identity()?,
                Delivery {
                    notice: notice.clone(),
                    recipients,
                    email: server.email.is_some(),
                },
            )
            .await,
    )?;
    Ok(())
}

async fn watched_recipients(
    client: &CellClient,
    candidates: Vec<(IdentityKey, Access)>,
    reason: Reason,
) -> Result<Vec<Recipient>> {
    let identities: Vec<_> = candidates
        .iter()
        .map(|(identity, _)| identity.clone())
        .collect();
    let ids = account_ids(client, &identities).await?;
    let missing: Vec<_> = identities
        .into_iter()
        .zip(&ids)
        .filter_map(|(identity, id)| id.is_none().then_some(identity))
        .collect();
    let count = missing.len();
    // A qualified author can start watching before ever opening the inbox.
    let fresh = if missing.is_empty() {
        Vec::new()
    } else {
        app::command_output(
            client
                .command::<RegisterAccounts>(Address::Directory, app::mutation_identity()?, missing)
                .await,
        )?
    };
    if fresh.len() != count {
        return Err(Error::CellContract(
            "Notification account registration returned an invalid count",
        ));
    }
    let mut fresh = fresh.into_iter();
    candidates
        .into_iter()
        .zip(ids)
        .map(|((_, access), id)| {
            let actor = id
                .or_else(|| fresh.next())
                .ok_or(Error::CellContract("Watch account is missing"))?;
            Ok(Recipient {
                actor,
                reason,
                access,
            })
        })
        .collect()
}

enum Progress {
    Ready(Option<watches::Key>),
    WaitingForFollow,
}

async fn enroll(
    server: &Server,
    repo: &Repository,
    client: &CellClient,
    scope: &DeliveryScope,
    source: FollowSource,
    candidates: Vec<(Recipient, IdentityKey)>,
) -> Result<()> {
    let Some(auth) = server.auth.as_ref() else {
        return Ok(());
    };
    // Group notices can be queued for offline readers. A durable follow
    // requires current signed access evidence for those same accounts.
    let enrolled: Vec<Option<IdentityKey>> = stream::iter(candidates)
        .map(|(candidate, identity)| async move {
            if candidate.access == Access::Direct
                || auth
                    .mention_group(&identity.subject, scope.group_grants(&identity))
                    .await?
                    .is_some()
            {
                Ok::<_, Error>(Some(identity))
            } else {
                Ok(None)
            }
        })
        .buffer_unordered(6)
        .try_collect()
        .await?;
    let recipients: Vec<_> = enrolled.into_iter().flatten().collect();
    if !recipients.is_empty() && scope.current(server, repo).await? {
        app::command_output(
            client
                .command::<Enroll>(
                    Address::Repository(repo.id),
                    app::mutation_identity()?,
                    Enrollment { source, recipients },
                )
                .await,
        )?;
    }
    Ok(())
}

async fn deliver(
    server: &Server,
    repo: &Repository,
    client: &CellClient,
    scope: &DeliveryScope,
    settings: &Settings,
    event: &Event,
) -> Result<Progress> {
    let mut notice = Notice {
        id: event.id,
        repository: repo.id,
        target: event.target.into(),
        created_at_ms: event.created_at_ms,
        sender: event.actor.name.clone(),
    };
    if let Some(watch) = event.watch {
        let Some(page) = app::query_output(
            client
                .query::<watches::ListWatchers>(
                    Address::Repository(repo.id),
                    None,
                    watches::Query {
                        subject: event.target.subject(),
                        activity: event.version,
                        before: watch.before,
                    },
                )
                .await,
        )?
        else {
            return Ok(Progress::WaitingForFollow);
        };
        let candidates: Vec<_> = page
            .recipients
            .into_iter()
            .filter_map(|actor| scope.access(&actor).map(|access| (actor, access)))
            .collect();
        if !candidates.is_empty() && scope.current(server, repo).await? {
            publish(
                server,
                client,
                &notice,
                watched_recipients(client, candidates, Reason::Subscribed).await?,
            )
            .await?;
        }
        return Ok(Progress::Ready(page.next));
    }
    let sender_key = IdentityKey {
        issuer: event.actor.issuer.clone(),
        subject: event.actor.subject.clone(),
    };
    let origin = if let Some(id) = event.transfer_origin {
        let Some(origin) = server.repositories.by_id(id) else {
            return Ok(Progress::Ready(None));
        };
        let Some(grants) = current_grants(server, &origin).await? else {
            return Ok(Progress::Ready(None));
        };
        let profiles = app::query_output(
            client
                .query::<LookupActors>(Address::Directory, None, vec![sender_key.clone()])
                .await,
        )?;
        let [profile] = profiles.as_slice() else {
            return Err(Error::CellContract(
                "Transfer sender lookup returned an invalid count",
            ));
        };
        if let Some(profile) = profile {
            notice.sender.clone_from(&profile.name);
        }
        let crate::cells::repository::collaboration::Target::Issue { number } = event.target else {
            return Err(Error::CellContract("Transfer notice must target an issue"));
        };
        notice.target = NoticeTarget::IssueTransfer { origin: id, number };
        Some((origin, grants))
    } else {
        None
    };
    let recipients: BTreeMap<_, _> = if !event.assignees.is_empty() {
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
        let origin_settings = if event
            .current
            .iter()
            .any(|mention| matches!(mention, Mention::Team { .. }))
            && let Some((origin, _)) = &origin
        {
            Some(app::query_output(
                client
                    .query::<mention_settings::Get>(Address::Repository(origin.id), None, ())
                    .await,
            )?)
        } else {
            None
        };
        let teams = team_members(
            client,
            event,
            origin_settings.as_ref().unwrap_or(settings),
            sender,
            scope.groups.is_empty().then_some(candidates.as_slice()),
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
    if recipients.is_empty() || !scope.current(server, repo).await? {
        return Ok(Progress::Ready(None));
    }
    let recipients: Vec<_> = recipients.into_iter().collect();
    for batch in recipients.chunks(MAX_RECIPIENTS) {
        let identities = app::query_output(
            client
                .query::<LookupIdentities>(
                    Address::Directory,
                    None,
                    batch.iter().map(|(actor, _)| *actor).collect(),
                )
                .await,
        )?;
        if identities.len() != batch.len() {
            return Err(Error::CellContract(
                "Notification recipient lookup returned an invalid count",
            ));
        }
        let mut candidates = Vec::new();
        let mut qualified = Vec::new();
        for (&(actor, reason), identity) in batch.iter().zip(identities) {
            if let Some(identity) = identity
                && let Some(access) = scope.access(&identity)
            {
                candidates.push(Recipient {
                    actor,
                    reason: if origin.is_some() {
                        Reason::IssueTransferred
                    } else {
                        reason
                    },
                    access,
                });
                qualified.push(identity);
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let unmuted = app::query_output(
            client
                .query::<watches::Unmuted>(Address::Repository(repo.id), None, qualified.clone())
                .await,
        )?;
        if unmuted.len() != candidates.len() {
            return Err(Error::CellContract(
                "Notification preference lookup returned an invalid count",
            ));
        }
        let mut recipients: Vec<_> = candidates
            .into_iter()
            .zip(qualified)
            .zip(unmuted)
            .filter_map(|(candidate, enabled)| enabled.then_some(candidate))
            .collect();
        if let Some((origin, origin_grants)) = &origin {
            let Some(destination_grants) = current_grants(server, repo).await? else {
                return Ok(Progress::Ready(None));
            };
            if destination_grants.revision != scope.revision {
                return Err(Error::PageChanged);
            }
            let checked: Vec<_> = stream::iter(recipients)
                .map(|(recipient, identity)| {
                    let destination_grants = &destination_grants;
                    async move {
                        let allowed =
                            recipient_has_grant(server, origin_grants, recipient.actor, &identity)
                                .await?
                                && recipient_has_grant(
                                    server,
                                    destination_grants,
                                    recipient.actor,
                                    &identity,
                                )
                                .await?;
                        Ok::<_, Error>(allowed.then_some((recipient, identity)))
                    }
                })
                .buffer_unordered(6)
                .try_collect()
                .await?;
            recipients = checked.into_iter().flatten().collect();
            let Some(fresh) = current_grants(server, origin).await? else {
                return Ok(Progress::Ready(None));
            };
            if fresh.revision != origin_grants.revision || !scope.current(server, repo).await? {
                return Err(Error::PageChanged);
            }
        }
        if scope.creator.as_ref().is_some_and(|creator| {
            recipients
                .iter()
                .any(|(recipient, _)| recipient.actor == creator.actor)
        }) && !scope.current(server, repo).await?
        {
            return Ok(Progress::Ready(None));
        }
        publish(
            server,
            client,
            &notice,
            recipients.iter().map(|(recipient, _)| *recipient).collect(),
        )
        .await?;
        if let Some(source) = event.follow {
            enroll(server, repo, client, scope, source, recipients).await?;
        }
    }
    Ok(Progress::Ready(None))
}

async fn deliver_release(
    server: &Server,
    repo: &Repository,
    client: &CellClient,
    scope: Option<&DeliveryScope>,
    event: watches::ReleaseEvent,
) -> Result<()> {
    let cursor = event.cursor();
    let mut next = None;
    if let Some(scope) = scope {
        let Some(page) = app::query_output(
            client
                .query::<watches::ReleaseWatchers>(Address::Repository(repo.id), None, cursor)
                .await,
        )?
        else {
            return Ok(());
        };
        let candidates: Vec<_> = page
            .recipients
            .into_iter()
            .filter_map(|actor| scope.access(&actor).map(|access| (actor, access)))
            .collect();
        if !candidates.is_empty() && scope.current(server, repo).await? {
            let notice = Notice {
                id: event.id,
                repository: repo.id,
                target: NoticeTarget::Release {
                    kind: ReleaseKind::Release,
                    number: event.number,
                },
                created_at_ms: event.published_at_ms,
                sender: event.publisher.name,
            };
            publish(
                server,
                client,
                &notice,
                watched_recipients(client, candidates, Reason::Release).await?,
            )
            .await?;
        }
        next = page.next;
    }
    app::command_output(
        client
            .command::<watches::AcknowledgeRelease>(
                Address::Repository(repo.id),
                app::mutation_identity()?,
                watches::AcknowledgeReleaseInput { cursor, next },
            )
            .await,
    )
}

pub(crate) async fn drain_repository(server: &Server, repo: &Repository) -> Result<()> {
    let cell = app::repository_cell(server, repo)?;
    let events = app::query_output(cell.client.query::<Pending>(&cell.target, None, ()).await)?;
    if events.discussions.is_empty() && events.releases.is_empty() {
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
                        snapshot.baseline,
                        if snapshot.fork_destination.is_some() {
                            crate::RepositoryAccess::Read
                        } else {
                            crate::RepositoryAccess::Admin
                        },
                        None,
                    )
                    .await?,
                );
            }
            if let Some(destination) = snapshot.fork_destination {
                members.extend(
                    crate::repository::access::organization_members(
                        server,
                        destination,
                        &[],
                        crate::catalog::RepositoryBaseline::None,
                        crate::RepositoryAccess::Admin,
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
            let mut ids: BTreeSet<_> = account_ids(&cell.client, &identities)
                .await?
                .into_iter()
                .flatten()
                .collect();
            let creator = fork_creator_delivery(server, snapshot).await?;
            if let Some(creator) = &creator {
                ids.insert(creator.actor);
            }
            // Group candidates are only visible when the inbox principal has a
            // current verified repository grant. No background session is used.
            Some(DeliveryScope {
                issuer: auth.issuer().to_owned(),
                subjects: identities.into_iter().map(|actor| actor.subject).collect(),
                direct: ids,
                revision: snapshot.revision,
                groups: if auth.is_oidc() {
                    snapshot.groups.clone()
                } else {
                    Vec::new()
                },
                creator,
            })
        }
        _ => None,
    };
    // A retry in one outbox must not prevent the other from advancing.
    let releases = async {
        for event in events.releases {
            deliver_release(server, repo, &cell.client, roster.as_ref(), event).await?;
        }
        Ok::<(), Error>(())
    };
    let discussions = async {
        if events.discussions.is_empty() {
            return Ok(());
        }
        let settings = app::query_output(
            cell.client
                .query::<mention_settings::Get>(&cell.target, None, ())
                .await,
        )?;
        for event in events.discussions {
            let progress = if let Some(scope) = &roster
                && (!scope.subjects.is_empty()
                    || !scope.groups.is_empty()
                    || scope.creator.is_some())
            {
                deliver(server, repo, &cell.client, scope, &settings, &event).await?
            } else {
                Progress::Ready(None)
            };
            let Progress::Ready(next) = progress else {
                continue;
            };
            app::command_output(
                cell.client
                    .command::<Acknowledge>(
                        &cell.target,
                        app::mutation_identity()?,
                        EventKey {
                            id: event.id,
                            created_at_ms: event.created_at_ms,
                            watch: event.watch,
                            next,
                            follow: event.follow,
                        },
                    )
                    .await,
            )?;
        }
        Ok(())
    };
    let (releases, discussions) = tokio::join!(releases, discussions);
    releases?;
    discussions
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
                        tracing::warn!(repository_id = %repo.id, %error, "notification delivery will be retried");
                    }
                }
            }
        }
        cursor = (cursor + count) % repositories.len();
    }
}
#[cfg(test)]
mod tests;
