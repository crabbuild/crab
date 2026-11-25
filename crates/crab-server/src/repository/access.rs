use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::Duration,
};

use futures_util::{StreamExt, TryStreamExt};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    app::{self, Error, Result},
    auth::Principal,
    catalog::{MembershipSnapshot, RepositoryApplicationState, RepositoryBaseline, TeamGrant},
    cells::{
        Address, CellClient,
        directory::{
            IdentityKey, LookupAccounts, LookupIdentities, LookupRoutingIdentities, RoutingLookup,
        },
        organization::{
            Caller, GetOrganization, Role,
            discovery::{GetLanding, Landing},
            teams::{
                repository_access::{
                    ActorTeams, RepositoryActor, RepositoryRosterQuery, RepositoryTeamNames,
                    RepositoryTeamQuery, RepositoryTeamRoster, RepositoryTeamsForActor,
                    VisibleRepositoryTeams,
                },
                review_routing::{Algorithm, ReviewRoster},
            },
        },
        repository::{
            MergeReviewers, RepositoryAuthor,
            review_requests::{automatic::TeamRecipients, routing},
        },
    },
    server::Server,
};

use super::Repository;

pub(crate) async fn actor_id(server: &Server, key: IdentityKey) -> Result<Option<Uuid>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let result = app::query_output(
        cells
            .query::<LookupAccounts>(Address::Directory, None, vec![key])
            .await,
    )?;
    match result.as_slice() {
        [id] => Ok(*id),
        _ => Err(Error::CellContract(
            "Account lookup returned an invalid result count",
        )),
    }
}

pub(crate) async fn organization_role(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
) -> Result<Option<Role>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<GetOrganization>(Address::Organization(organization), None, Caller { actor })
        .await
    {
        Ok(result) => Ok(result.output.map(|view| view.role)),
        Err(crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(crate) async fn organization_landing(
    server: &Server,
    organization: Uuid,
    actor: Uuid,
) -> Result<Option<Landing>> {
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    match cells
        .query::<GetLanding>(Address::Organization(organization), None, Caller { actor })
        .await
    {
        Ok(result) => Ok(result.output),
        Err(crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied) => Ok(None),
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(crate) fn organization_access(
    grant: &ActorTeams,
    teams: &[TeamGrant],
    baseline: RepositoryBaseline,
) -> Option<RepositoryAccess> {
    organization_access_with_owner(grant, teams, baseline, RepositoryAccess::Admin)
}

pub(crate) fn organization_access_with_owner(
    grant: &ActorTeams,
    teams: &[TeamGrant],
    baseline: RepositoryBaseline,
    owner_access: RepositoryAccess,
) -> Option<RepositoryAccess> {
    if grant.owner {
        return Some(owner_access);
    }
    let baseline = grant
        .member
        .then(|| baseline.resolve(grant.baseline))
        .flatten();
    teams
        .iter()
        .filter(|team| grant.teams.binary_search(&team.team).is_ok())
        .map(|team| team.access)
        .chain(baseline)
        .max()
}

pub(crate) fn snapshot_organization_access(
    snapshot: &MembershipSnapshot,
    grants: &HashMap<Uuid, ActorTeams>,
) -> Option<RepositoryAccess> {
    if snapshot
        .fork_destination
        .and_then(|id| grants.get(&id))
        .is_some_and(|grant| grant.owner)
    {
        return Some(RepositoryAccess::Admin);
    }
    let source = snapshot.organization.as_ref()?;
    let grant = grants.get(&source.id)?;
    let owner_access = if snapshot.fork_destination.is_some() {
        RepositoryAccess::Read
    } else {
        RepositoryAccess::Admin
    };
    organization_access_with_owner(grant, &source.teams, snapshot.baseline, owner_access)
}

pub(crate) async fn organization_grants(
    server: &Server,
    principal: &Principal,
    organizations: HashMap<Uuid, bool>,
) -> Result<HashMap<Uuid, ActorTeams>> {
    if organizations.is_empty() {
        return Ok(HashMap::new());
    }
    let identity = principal.identity().ok_or(Error::NotFound)?;
    let key = IdentityKey {
        issuer: identity.issuer,
        subject: identity.subject,
    };
    organization_grants_for_identity(server, key, organizations).await
}

pub(crate) async fn organization_grants_for_identity(
    server: &Server,
    key: IdentityKey,
    organizations: HashMap<Uuid, bool>,
) -> Result<HashMap<Uuid, ActorTeams>> {
    let Some(actor) = actor_id(server, key).await? else {
        return Ok(HashMap::new());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    futures_util::stream::iter(
        organizations
            .into_iter()
            .map(|(organization, include_teams)| {
                let cells = &cells;
                async move {
                    let grant =
                        organization_grant(cells, organization, actor, include_teams).await?;
                    Ok((organization, grant))
                }
            }),
    )
    .buffer_unordered(8)
    .try_collect()
    .await
}

pub(crate) async fn organization_grant(
    cells: &CellClient,
    organization: Uuid,
    actor: Uuid,
    include_teams: bool,
) -> Result<ActorTeams> {
    match cells
        .query::<RepositoryTeamsForActor>(
            Address::Organization(organization),
            None,
            RepositoryActor {
                actor,
                include_teams,
            },
        )
        .await
    {
        Ok(result) => Ok(result.output),
        Err(crate::cells::Error::NotInitialized | crate::cells::Error::AccessDenied) => {
            Ok(ActorTeams::default())
        }
        Err(error) => Err(Error::Cell(error)),
    }
}

pub(crate) async fn visible_team_names(
    server: &Server,
    principal: &Principal,
    organization: Uuid,
    teams: Vec<Uuid>,
) -> Result<VisibleRepositoryTeams> {
    let key = app::account(principal)?;
    let Some(actor) = actor_id(server, key).await? else {
        return Ok(VisibleRepositoryTeams::default());
    };
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    app::query_output(
        cells
            .query::<RepositoryTeamNames>(
                Address::Organization(organization),
                None,
                RepositoryTeamQuery { actor, teams },
            )
            .await,
    )
}

pub(crate) async fn organization_members(
    server: &Server,
    organization: Uuid,
    teams: &[TeamGrant],
    baseline: RepositoryBaseline,
    owner_access: RepositoryAccess,
    principal: Option<&Principal>,
) -> Result<Vec<RepositoryMember>> {
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let roster = app::query_output(
        cells
            .query::<RepositoryTeamRoster>(
                Address::Organization(organization),
                None,
                RepositoryRosterQuery {
                    teams: teams.iter().map(|grant| grant.team).collect(),
                    baseline,
                },
            )
            .await,
    )?;
    let mut grants = BTreeMap::new();
    if let Some((access, actors)) = roster.baseline {
        grants.extend(actors.into_iter().map(|actor| (actor, access)));
    }
    for (team, actor) in roster.members {
        let access = teams
            .iter()
            .find(|grant| grant.team == team)
            .ok_or(Error::CellContract(
                "Team roster returned an unrelated team",
            ))?
            .access;
        grants
            .entry(actor)
            .and_modify(|current: &mut RepositoryAccess| *current = (*current).max(access))
            .or_insert(access);
    }
    for owner in roster.owners {
        grants.insert(owner, owner_access);
    }

    let ids: Vec<_> = grants.keys().copied().collect();
    let current = principal.and_then(Principal::identity);
    let mut members = Vec::with_capacity(ids.len());
    for chunk in ids.chunks(100) {
        let identities = app::query_output(
            cells
                .query::<LookupIdentities>(Address::Directory, None, chunk.to_vec())
                .await,
        )?;
        if identities.len() != chunk.len() || identities.iter().any(Option::is_none) {
            return Err(Error::CellContract(
                "Organization member is missing a Directory account",
            ));
        }
        members.extend(
            chunk
                .iter()
                .zip(identities.into_iter().flatten())
                .filter(|(_, key)| key.issuer == issuer)
                .map(|(id, key)| RepositoryMember {
                    name: current
                        .as_ref()
                        .filter(|actor| actor.issuer == issuer && actor.subject == key.subject)
                        .map_or_else(|| "Organization member".into(), |actor| actor.name.clone()),
                    subject: key.subject,
                    access: grants[id],
                }),
        );
    }
    Ok(members)
}

pub(crate) async fn team_review_plan(
    server: &Server,
    membership: &app::Membership,
    team: Uuid,
    eligibility: Option<&MergeReviewers>,
) -> Result<TeamRecipients> {
    use crate::cells::repository::review_requests::teams::MAX_REVIEW_RECIPIENTS;

    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let roster = app::query_output(
        cells
            .query::<ReviewRoster>(
                Address::Organization(membership.organization.ok_or(Error::NotFound)?),
                None,
                team,
            )
            .await,
    )?;
    if roster.members.len() > MAX_REVIEW_RECIPIENTS {
        return Err(Error::Invalid(
            "Team review requests support up to 256 team members",
        ));
    }
    let automatic = roster.settings.algorithm != Algorithm::Disabled;
    let mut members = Vec::with_capacity(roster.members.len());
    for chunk in roster.members.chunks(100) {
        let identities = app::query_output(
            cells
                .query::<LookupRoutingIdentities>(
                    Address::Directory,
                    None,
                    RoutingLookup {
                        actors: chunk.to_vec(),
                        check_availability: automatic,
                    },
                )
                .await,
        )?;
        if identities.len() != chunk.len() || identities.iter().any(Option::is_none) {
            return Err(Error::CellContract(
                "Team member is missing a Directory account",
            ));
        }
        members.extend(
            chunk
                .iter()
                .copied()
                .zip(identities.into_iter().flatten())
                .filter_map(|(actor, account)| {
                    (account.identity.issuer == issuer).then_some((
                        actor,
                        account.identity.subject,
                        account.busy,
                    ))
                }),
        );
    }
    if !automatic {
        return Ok(TeamRecipients {
            team,
            recipients: members.into_iter().map(|(_, subject, _)| subject).collect(),
            routing: None,
        });
    }
    let current;
    let eligibility = match eligibility {
        Some(eligibility) => eligibility,
        None => {
            current = membership.merge_reviewers(server).await?;
            &current
        }
    };
    let options = roster.settings.options;
    let mut count_existing = options.count_existing.then(Vec::new);
    let mut candidates = Vec::new();
    for (actor, subject, busy) in members {
        if !eligibility.contains(issuer, &subject) {
            continue;
        }
        if let Some(eligible) = &mut count_existing {
            eligible.push(subject.clone());
        }
        if busy || roster.settings.excluded.binary_search(&actor).is_ok() {
            continue;
        }
        let name = membership
            .members
            .iter()
            .find(|member| member.subject == subject)
            .map_or_else(
                || "Organization member".into(),
                |member| member.name.clone(),
            );
        candidates.push(RepositoryAuthor {
            issuer: issuer.to_owned(),
            subject,
            name,
        });
    }
    Ok(TeamRecipients {
        team,
        recipients: Vec::new(),
        routing: Some(routing::Plan {
            algorithm: roster.settings.algorithm,
            reviewers: roster.settings.reviewers,
            candidates,
            count_existing,
            keep_team: options.keep_team,
            other_workload: BTreeMap::new(),
            other_rotation: BTreeMap::new(),
        }),
    })
}

pub(crate) async fn add_review_history(
    server: &Server,
    repo: &Repository,
    teams: &mut [TeamRecipients],
) {
    let plans: Vec<_> = teams
        .iter_mut()
        .filter_map(|team| {
            team.routing
                .as_mut()
                .filter(|plan| !plan.candidates.is_empty())
                .map(|plan| (team.team, plan))
        })
        .collect();
    if plans.is_empty() {
        return;
    }
    let workload: BTreeSet<_> = plans
        .iter()
        .filter(|(_, plan)| plan.algorithm == Algorithm::FewestOpen)
        .flat_map(|(_, plan)| {
            plan.candidates
                .iter()
                .map(|candidate| candidate.subject.clone())
        })
        .collect();
    let subjects_by_team = plans
        .iter()
        .map(|(team, plan)| {
            let subjects = plan
                .candidates
                .iter()
                .map(|candidate| candidate.subject.clone())
                .collect();
            (*team, subjects)
        })
        .collect();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        other_review_history(
            server,
            repo,
            workload.into_iter().collect(),
            subjects_by_team,
        ),
    )
    .await;
    let mut history = match result {
        Ok(Ok(history)) => history,
        other => {
            // Ranking is advisory. A sibling outage must not prevent the PR
            // or team request from committing, or silently bias the choice.
            tracing::warn!(repository = %repo.id, outcome = ?other, "team review assignment history unavailable");
            for (_, plan) in plans {
                plan.candidates.clear();
            }
            return;
        }
    };
    for (team, plan) in plans {
        plan.other_rotation = history.teams.remove(&team).unwrap_or_default();
        if plan.algorithm == Algorithm::FewestOpen {
            for candidate in &plan.candidates {
                if let Some(load) = history.workload.get(&candidate.subject) {
                    plan.other_workload.insert(candidate.subject.clone(), *load);
                }
            }
        }
    }
}

async fn other_review_history(
    server: &Server,
    repo: &Repository,
    workload: Vec<String>,
    teams: BTreeMap<Uuid, Vec<String>>,
) -> Result<routing::History> {
    let organization = repo.organization.ok_or(Error::NotFound)?;
    let issuer = server.auth.as_ref().ok_or(Error::NotFound)?.issuer();
    let cells = server.cells().ok_or(Error::CellUnavailable)?;
    let catalog = server.catalog().ok_or(Error::NotFound)?;
    let (document, _) = catalog
        .load()
        .await
        .map_err(|error| Error::Membership(Box::new(error)))?;
    let input = routing::HistoryQuery {
        issuer: issuer.to_owned(),
        workload,
        teams,
    };
    // Catalog ownership scopes the private aggregate. Only the destination
    // repository may turn today's eligible candidates into actual requests.
    futures_util::stream::iter(document.repositories.into_iter().filter_map(|record| {
        (record.id != repo.id
            && record.organization_id() == Some(organization)
            && record.application == RepositoryApplicationState::CellReady)
            .then_some(record.id)
    }))
    .map(|repository| {
        let input = input.clone();
        let cells = &cells;
        async move {
            app::query_output(
                cells
                    .query::<routing::ReadHistory>(Address::Repository(repository), None, input)
                    .await,
            )
        }
    })
    .buffer_unordered(8)
    .try_fold(routing::History::default(), |mut history, sibling| async {
        history.add(sibling);
        Ok(history)
    })
    .await
}
